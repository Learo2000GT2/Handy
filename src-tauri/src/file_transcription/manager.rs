//! The GUI's file transcription queue: one worker thread runs the jobs one
//! at a time, in the order they were added, publishes their state as
//! `FileTranscriptionEvent`s and saves results to history.
//!
//! Jobs live only in memory. The window hides on close, so jobs keep running
//! until the app exits; only what was saved to history outlives the app.

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::formats;
use crate::file_transcription::lemonade::{self, LemonadeJobConfig, LEMONADE_MODEL_PREFIX};
use crate::file_transcription::pipeline::{
    self, PipelineEngine, PipelineError, PipelineEvent, PipelineOptions, PipelineOutput,
    TranscriptBuilder, TranscriptMeta,
};
use crate::file_transcription::speaker_model;
use crate::file_transcription::types::{
    DecoderKind, FileJobError, FileJobErrorCode, FileJobSnapshot, FileJobStatus, FileTranscript,
    FileTranscriptionEvent, FileTranscriptionOptions, SpeakerJobState, SpeakerSkipReason,
    TranscriptStatus,
};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{AppSettings, FileTranscriptionEngine};
use chrono::Utc;
use log::{error, info, warn};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

/// Progress-only `Updated` events are sent at most this often per job.
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(250);
/// Finished jobs kept for the queue view; older ones are evicted.
const MAX_FINISHED_JOBS: usize = 20;
/// `status_detail` of the transcript of a job that is still running.
const IN_PROGRESS_DETAIL: &str = "Transcription in progress";

pub struct FileTranscriptionManager {
    app: AppHandle,
    tm: Arc<TranscriptionManager>,
    history: Arc<HistoryManager>,
    state: Mutex<JobTable>,
    /// Signalled when a job is queued.
    wake: Condvar,
    next_id: AtomicU64,
}

impl FileTranscriptionManager {
    /// Spawns the worker thread "file-transcription".
    pub fn new(
        app: &AppHandle,
        tm: Arc<TranscriptionManager>,
        history: Arc<HistoryManager>,
    ) -> Arc<Self> {
        let manager = Arc::new(Self {
            app: app.clone(),
            tm,
            history,
            state: Mutex::new(JobTable::default()),
            wake: Condvar::new(),
            next_id: AtomicU64::new(1),
        });

        let worker = Arc::clone(&manager);
        std::thread::Builder::new()
            .name("file-transcription".to_string())
            .spawn(move || worker.run_worker())
            .expect("Failed to spawn the file transcription worker");

        manager
    }

    /// Validates each path (exists + is_file → else Failed NotFound;
    /// route_for_path → else Failed UnsupportedFormat). Valid ones are Queued.
    /// Emits Updated per job. Never blocks on work.
    pub fn enqueue(
        &self,
        paths: Vec<String>,
        options: FileTranscriptionOptions,
    ) -> Vec<FileJobSnapshot> {
        let enqueued_at = Utc::now().timestamp_millis();
        // Shown while queued; fixed from the settings when the job starts.
        let engine = crate::settings::get_settings(&self.app).file_transcription_engine;
        // Touch the file system before taking the lock.
        let checked: Vec<(String, Result<(), FileJobError>)> = paths
            .into_iter()
            .map(|path| {
                let validation = validate_path(Path::new(&path), |path| {
                    formats::route_for_path(path).is_some()
                });
                (path, validation)
            })
            .collect();

        let (refreshed, snapshots, evicted) = {
            let mut state = self.lock_state();
            let refreshed = state.refresh_queued_engine(engine);
            let snapshots: Vec<FileJobSnapshot> = checked
                .into_iter()
                .map(|(path, validation)| {
                    let job_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    state.add(
                        job_id,
                        path,
                        options.clone(),
                        engine,
                        validation,
                        enqueued_at,
                    )
                })
                .collect();
            (
                refreshed,
                snapshots,
                state.evict_finished(MAX_FINISHED_JOBS),
            )
        };
        self.wake.notify_one();

        for job in refreshed {
            self.emit(FileTranscriptionEvent::Updated { job });
        }
        for snapshot in &snapshots {
            match &snapshot.error {
                Some(error) => warn!("Not transcribing {}: {}", snapshot.path, error.message),
                None => info!("Queued file transcription job {}", snapshot.job_id),
            }
            self.emit(FileTranscriptionEvent::Updated {
                job: snapshot.clone(),
            });
        }
        self.emit_removed(evicted);
        snapshots
    }

    /// Queued → Cancelled at once. Running → token cancel + tm.cancel_file_work(&token);
    /// the status changes when the pipeline returns. Finished/unknown → Err.
    pub fn cancel(&self, job_id: u64) -> Result<(), String> {
        let cancellation = self.lock_state().cancel(job_id)?;
        match cancellation {
            Cancellation::Dequeued(snapshot) => {
                self.emit(FileTranscriptionEvent::Updated { job: snapshot });
            }
            Cancellation::Running(token) => {
                info!("Cancelling file transcription job {}", job_id);
                self.tm.cancel_file_work(&token);
            }
        }
        Ok(())
    }

    pub fn cancel_all(&self) {
        let (dequeued, running) = self.lock_state().cancel_all();
        for snapshot in dequeued {
            self.emit(FileTranscriptionEvent::Updated { job: snapshot });
        }
        for token in running {
            self.tm.cancel_file_work(&token);
        }
    }

    /// Enqueue order. Queued jobs show the engine selected now.
    pub fn list(&self) -> Vec<FileJobSnapshot> {
        let engine = crate::settings::get_settings(&self.app).file_transcription_engine;
        let mut state = self.lock_state();
        state.refresh_queued_engine(engine);
        state.snapshots()
    }

    /// Shows `engine` (the engine setting) on the queued jobs, which start
    /// on whatever engine is selected then; emits Updated for each one that
    /// changed. Call it when the engine setting changes; the manager also
    /// calls it whenever it lists jobs, queues jobs or starts one.
    pub fn refresh_queued_engine(&self, engine: FileTranscriptionEngine) {
        let refreshed = self.lock_state().refresh_queued_engine(engine);
        for job in refreshed {
            self.emit(FileTranscriptionEvent::Updated { job });
        }
    }

    /// Only finished jobs; frees the result; emits Removed.
    pub fn dismiss(&self, job_id: u64) -> Result<(), String> {
        self.lock_state().dismiss(job_id)?;
        self.emit(FileTranscriptionEvent::Removed { job_id });
        Ok(())
    }

    /// Finished: stored result. Running: what is done so far (status Partial,
    /// status_detail "Transcription in progress"). None if unknown or nothing yet.
    pub fn transcript(&self, job_id: u64) -> Option<FileTranscript> {
        self.lock_state().transcript(job_id, Utc::now().timestamp())
    }

    /// Applies `edit` to a finished job's stored result (`JobRecord.result`)
    /// and returns the updated transcript. Err if the job is unknown, still
    /// running, or has no result.
    ///
    /// `edit` runs with the job table locked, so it must not block. A
    /// failed edit leaves the stored result unchanged.
    pub fn edit_job_transcript(
        &self,
        job_id: u64,
        edit: impl FnOnce(&mut FileTranscript) -> Result<(), String>,
    ) -> Result<FileTranscript, String> {
        self.lock_state().edit_result(job_id, edit)
    }

    /// The History entry a finished job's result was saved as. Err like
    /// [`Self::edit_job_transcript`] when there is no result to edit.
    pub fn job_history_id(&self, job_id: u64) -> Result<Option<i64>, String> {
        self.lock_state()
            .result_mut(job_id)
            .map(|result| result.history_id)
    }

    /// After the speakers of History entry `stored.history_id` were renamed,
    /// gives every finished job whose result was saved as that entry the
    /// entry's labels and text, so the History rename shows there too and a
    /// later rename from the job keeps it. Emits Updated for each such job.
    pub fn sync_history_speakers(&self, stored: &FileTranscript) {
        let Some(history_id) = stored.history_id else {
            return;
        };
        let synced = self.lock_state().sync_history_speakers(history_id, stored);
        for job in synced {
            self.emit(FileTranscriptionEvent::Updated { job });
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, JobTable> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: FileTranscriptionEvent) {
        if let Err(e) = event.emit(&self.app) {
            error!("Failed to emit file-transcription-event: {}", e);
        }
    }

    fn emit_removed(&self, job_ids: Vec<u64>) {
        for job_id in job_ids {
            self.emit(FileTranscriptionEvent::Removed { job_id });
        }
    }

    fn run_worker(&self) {
        loop {
            let (job, settings) = self.next_job();
            self.emit(FileTranscriptionEvent::Updated {
                job: job.snapshot.clone(),
            });
            self.refresh_queued_engine(settings.file_transcription_engine);

            self.run_job(&job, &settings);

            crate::memory::trim_freed_memory();
            let evicted = self.lock_state().evict_finished(MAX_FINISHED_JOBS);
            self.emit_removed(evicted);
        }
    }

    /// Waits for a queued job and starts it, with the settings it runs with:
    /// the engine and model are fixed when a job starts, not when it was
    /// queued.
    fn next_job(&self) -> (StartedJob, AppSettings) {
        loop {
            // Read outside the lock, and again after every wake-up.
            let settings = crate::settings::get_settings(&self.app);
            let mut state = self.lock_state();
            if let Some(job) = state.start_next(settings.file_transcription_engine) {
                return (job, settings);
            }
            // Re-check (with fresh settings) after a wake-up; a job queued in
            // between is found by that check, so no wake-up is lost.
            let state = self
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            drop(state);
        }
    }

    /// Runs one job to its end. The job table is locked only briefly to
    /// record progress: never while the pipeline works or an event is sent.
    fn run_job(&self, job: &StartedJob, settings: &AppSettings) {
        let (engine, model_id) = job_engine(settings);
        info!(
            "Starting file transcription job {} with model {}",
            job.job_id, model_id
        );

        let mut on_event = |event: PipelineEvent| {
            let events = self
                .lock_state()
                .apply(job.job_id, &model_id, event, Instant::now());
            for event in events {
                self.emit(event);
            }
        };
        let speaker_model = if job.options.label_speakers {
            let installed = speaker_model::installed_speaker_model(&self.app);
            if installed.is_none() {
                info!(
                    "Not labelling speakers of job {}: the speaker model isn't installed",
                    job.job_id
                );
                on_event(PipelineEvent::Speakers(SpeakerJobState::Skipped {
                    reason: SpeakerSkipReason::ModelMissing,
                }));
            }
            installed
        } else {
            None
        };
        let options = PipelineOptions {
            engine,
            max_chunk_ms: None,
            ffmpeg: None,
            skip_silent_chunks: true,
            speaker_model,
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            pipeline::run_pipeline(
                &self.tm,
                Path::new(&job.path),
                &options,
                &job.token,
                &mut on_event,
            )
        }));

        let finish = match result {
            Ok(Ok(output)) => {
                info!("File transcription job {} completed", job.job_id);
                let decoder = output.decoder;
                let transcript = self.store_result(job, &model_id, output);
                JobFinish {
                    status: FileJobStatus::Completed,
                    error: None,
                    decoder: Some(decoder),
                    result: Some(transcript),
                }
            }
            Ok(Err(PipelineError::Cancelled { partial })) => {
                info!("File transcription job {} cancelled", job.job_id);
                // Kept for viewing and export, but never saved to history.
                let result = partial
                    .filter(|partial| !partial.transcript.paragraphs.is_empty())
                    .map(|partial| self.finish_transcript(partial.transcript, &model_id));
                JobFinish {
                    status: FileJobStatus::Cancelled,
                    error: None,
                    decoder: None,
                    result,
                }
            }
            Ok(Err(PipelineError::Failed {
                code,
                message,
                partial,
            })) => {
                warn!(
                    "File transcription job {} failed ({:?}): {}",
                    job.job_id, code, message
                );
                let result = partial
                    .filter(|partial| !partial.transcript.paragraphs.is_empty())
                    .map(|partial| self.store_result(job, &model_id, *partial));
                JobFinish {
                    status: FileJobStatus::Failed,
                    error: Some(FileJobError { code, message }),
                    decoder: None,
                    result,
                }
            }
            Err(_) => {
                error!("File transcription job {} panicked", job.job_id);
                JobFinish {
                    status: FileJobStatus::Failed,
                    error: Some(FileJobError {
                        code: FileJobErrorCode::Internal,
                        message: "File transcription stopped unexpectedly".to_string(),
                    }),
                    decoder: None,
                    result: None,
                }
            }
        };

        let snapshot = self.lock_state().finish(job.job_id, finish);
        if let Some(snapshot) = snapshot {
            self.emit(FileTranscriptionEvent::Updated { job: snapshot });
        }
    }

    /// Completes a pipeline result and saves it to history when the job asked
    /// for that and there is text to save.
    fn store_result(
        &self,
        job: &StartedJob,
        model_id: &str,
        output: PipelineOutput,
    ) -> FileTranscript {
        let mut transcript = self.finish_transcript(output.transcript, model_id);
        if job.options.save_to_history && !transcript.text.trim().is_empty() {
            match self.history.save_file_entry(
                &transcript,
                Some(output.source_size_bytes),
                output.source_modified_at,
            ) {
                Ok(entry) => transcript.history_id = Some(entry.id),
                Err(e) => error!(
                    "Failed to save the transcript of {} to history: {:#}",
                    transcript.source_name, e
                ),
            }
        }
        transcript
    }

    fn finish_transcript(&self, mut transcript: FileTranscript, model_id: &str) -> FileTranscript {
        transcript.model_name = lemonade::display_model_name(model_id).or_else(|| {
            self.app
                .try_state::<Arc<ModelManager>>()
                .and_then(|models| models.get_model_info(model_id))
                .map(|info| info.name)
        });
        transcript.created_at = Utc::now().timestamp();
        transcript
    }
}

/// The pipeline engine for a job started with `settings`, and the model id
/// it reports ("small", "lemonade:Whisper-Base").
fn job_engine(settings: &AppSettings) -> (PipelineEngine, String) {
    match settings.file_transcription_engine {
        FileTranscriptionEngine::Handy => (
            PipelineEngine::Handy {
                model_id: settings.selected_model.clone(),
                device_index: None,
            },
            settings.selected_model.clone(),
        ),
        FileTranscriptionEngine::Lemonade => {
            let config = LemonadeJobConfig::from_settings(&settings.lemonade);
            let model_id = format!("{}{}", LEMONADE_MODEL_PREFIX, config.model);
            (
                PipelineEngine::Lemonade {
                    config,
                    settings: settings.clone(),
                },
                model_id,
            )
        }
    }
}

/// `Ok` when `path` is an existing regular file whose type `is_supported`
/// accepts.
fn validate_path(path: &Path, is_supported: impl Fn(&Path) -> bool) -> Result<(), FileJobError> {
    if !path.is_file() {
        return Err(FileJobError {
            code: FileJobErrorCode::NotFound,
            message: format!("{} does not exist or is not a file", path.display()),
        });
    }
    if !is_supported(path) {
        let message = match path.extension() {
            Some(extension) => format!(
                "Unsupported file type: .{}",
                extension.to_string_lossy().to_lowercase()
            ),
            None => "Files without an extension are not supported".to_string(),
        };
        return Err(FileJobError {
            code: FileJobErrorCode::UnsupportedFormat,
            message,
        });
    }
    Ok(())
}

fn is_finished(status: FileJobStatus) -> bool {
    matches!(
        status,
        FileJobStatus::Completed | FileJobStatus::Cancelled | FileJobStatus::Failed
    )
}

struct JobRecord {
    snapshot: FileJobSnapshot,
    options: FileTranscriptionOptions,
    token: WorkToken,
    /// Chunks finished so far, for the live view while the job runs.
    transcript: TranscriptBuilder,
    /// The final (or partial) transcript once the job finished.
    result: Option<FileTranscript>,
    last_progress_emit: Option<Instant>,
}

/// A job the worker just took off the queue.
struct StartedJob {
    job_id: u64,
    path: String,
    options: FileTranscriptionOptions,
    token: WorkToken,
    snapshot: FileJobSnapshot,
}

enum Cancellation {
    /// The job was queued and is now cancelled.
    Dequeued(FileJobSnapshot),
    /// The job is running; its work must be cancelled with this token.
    Running(WorkToken),
}

/// How a job ended.
struct JobFinish {
    status: FileJobStatus,
    error: Option<FileJobError>,
    decoder: Option<DecoderKind>,
    result: Option<FileTranscript>,
}

/// All jobs in enqueue order. Pure state, so it can be tested without an
/// app; the manager does the locking, the work and the events.
#[derive(Default)]
struct JobTable {
    jobs: Vec<JobRecord>,
}

impl JobTable {
    fn record(&self, job_id: u64) -> Option<&JobRecord> {
        self.jobs.iter().find(|job| job.snapshot.job_id == job_id)
    }

    fn record_mut(&mut self, job_id: u64) -> Option<&mut JobRecord> {
        self.jobs
            .iter_mut()
            .find(|job| job.snapshot.job_id == job_id)
    }

    /// Adds a job: `Queued`, or `Failed` with the validation error.
    fn add(
        &mut self,
        job_id: u64,
        path: String,
        options: FileTranscriptionOptions,
        engine: FileTranscriptionEngine,
        validation: Result<(), FileJobError>,
        enqueued_at: i64,
    ) -> FileJobSnapshot {
        let error = validation.err();
        let speakers = options.label_speakers.then_some(SpeakerJobState::Pending);
        let snapshot = FileJobSnapshot {
            job_id,
            file_name: pipeline::source_display_name(Path::new(&path)),
            path,
            status: if error.is_some() {
                FileJobStatus::Failed
            } else {
                FileJobStatus::Queued
            },
            duration_ms: None,
            processed_ms: 0,
            chunks_done: 0,
            decoder: None,
            model_id: None,
            engine,
            speakers,
            error,
            has_transcript: false,
            transcript_status: None,
            history_id: None,
            enqueued_at,
        };
        self.jobs.push(JobRecord {
            snapshot: snapshot.clone(),
            options,
            token: WorkToken::new(),
            transcript: TranscriptBuilder::default(),
            result: None,
            last_progress_emit: None,
        });
        snapshot
    }

    /// Takes the oldest queued job and marks it `Preparing` on `engine`.
    fn start_next(&mut self, engine: FileTranscriptionEngine) -> Option<StartedJob> {
        let record = self
            .jobs
            .iter_mut()
            .find(|job| job.snapshot.status == FileJobStatus::Queued)?;
        record.snapshot.status = FileJobStatus::Preparing;
        record.snapshot.engine = engine;
        record.last_progress_emit = None;
        Some(StartedJob {
            job_id: record.snapshot.job_id,
            path: record.snapshot.path.clone(),
            options: record.options.clone(),
            token: record.token.clone(),
            snapshot: record.snapshot.clone(),
        })
    }

    /// Sets `engine` on the queued jobs; returns the snapshots that changed.
    fn refresh_queued_engine(&mut self, engine: FileTranscriptionEngine) -> Vec<FileJobSnapshot> {
        self.jobs
            .iter_mut()
            .filter(|job| job.snapshot.status == FileJobStatus::Queued)
            .filter(|job| job.snapshot.engine != engine)
            .map(|job| {
                job.snapshot.engine = engine;
                job.snapshot.clone()
            })
            .collect()
    }

    fn cancel(&mut self, job_id: u64) -> Result<Cancellation, String> {
        let record = self
            .record_mut(job_id)
            .ok_or_else(|| format!("Unknown file transcription job {}", job_id))?;
        match record.snapshot.status {
            FileJobStatus::Queued => {
                record.snapshot.status = FileJobStatus::Cancelled;
                Ok(Cancellation::Dequeued(record.snapshot.clone()))
            }
            FileJobStatus::Preparing | FileJobStatus::Waiting | FileJobStatus::Transcribing => {
                record.token.cancel();
                Ok(Cancellation::Running(record.token.clone()))
            }
            FileJobStatus::Completed | FileJobStatus::Cancelled | FileJobStatus::Failed => Err(
                format!("File transcription job {} has already finished", job_id),
            ),
        }
    }

    /// Cancels every unfinished job: returns the dequeued jobs' snapshots and
    /// the running jobs' tokens.
    fn cancel_all(&mut self) -> (Vec<FileJobSnapshot>, Vec<WorkToken>) {
        let mut dequeued = Vec::new();
        let mut running = Vec::new();
        let ids: Vec<u64> = self.jobs.iter().map(|job| job.snapshot.job_id).collect();
        for job_id in ids {
            match self.cancel(job_id) {
                Ok(Cancellation::Dequeued(snapshot)) => dequeued.push(snapshot),
                Ok(Cancellation::Running(token)) => running.push(token),
                Err(_) => {}
            }
        }
        (dequeued, running)
    }

    fn snapshots(&self) -> Vec<FileJobSnapshot> {
        self.jobs.iter().map(|job| job.snapshot.clone()).collect()
    }

    fn dismiss(&mut self, job_id: u64) -> Result<(), String> {
        let index = self
            .jobs
            .iter()
            .position(|job| job.snapshot.job_id == job_id)
            .ok_or_else(|| format!("Unknown file transcription job {}", job_id))?;
        if !is_finished(self.jobs[index].snapshot.status) {
            return Err(format!(
                "File transcription job {} is still running; cancel it first",
                job_id
            ));
        }
        self.jobs.remove(index);
        Ok(())
    }

    /// Applies `edit` to a finished job's result; see
    /// [`FileTranscriptionManager::edit_job_transcript`].
    fn edit_result(
        &mut self,
        job_id: u64,
        edit: impl FnOnce(&mut FileTranscript) -> Result<(), String>,
    ) -> Result<FileTranscript, String> {
        let result = self.result_mut(job_id)?;
        let mut edited = result.clone();
        edit(&mut edited)?;
        *result = edited.clone();
        Ok(edited)
    }

    /// A finished job's result; Err if the job is unknown, still running or
    /// has no result.
    fn result_mut(&mut self, job_id: u64) -> Result<&mut FileTranscript, String> {
        let record = self
            .record_mut(job_id)
            .ok_or_else(|| format!("Unknown file transcription job {}", job_id))?;
        if !is_finished(record.snapshot.status) {
            return Err(format!(
                "File transcription job {} is still running",
                job_id
            ));
        }
        record
            .result
            .as_mut()
            .ok_or_else(|| format!("File transcription job {} has no transcript", job_id))
    }

    /// Copies the speaker labels and text of History entry `history_id`
    /// (`stored`) into the results saved as that entry; returns those jobs'
    /// snapshots.
    fn sync_history_speakers(
        &mut self,
        history_id: i64,
        stored: &FileTranscript,
    ) -> Vec<FileJobSnapshot> {
        self.jobs
            .iter_mut()
            .filter_map(|job| {
                let result = job
                    .result
                    .as_mut()
                    .filter(|result| result.history_id == Some(history_id))?;
                result.speakers = stored.speakers.clone();
                result.text = stored.text.clone();
                Some(job.snapshot.clone())
            })
            .collect()
    }

    fn transcript(&self, job_id: u64, now_secs: i64) -> Option<FileTranscript> {
        let record = self.record(job_id)?;
        if is_finished(record.snapshot.status) {
            return record.result.clone();
        }
        if !record.transcript.has_text() {
            return None;
        }
        let snapshot = &record.snapshot;
        let source_path = pipeline::source_path_string(Path::new(&snapshot.path));
        let meta = TranscriptMeta {
            source_path: &source_path,
            source_name: &snapshot.file_name,
            model_id: snapshot.model_id.as_deref().unwrap_or_default(),
            duration_ms: snapshot.duration_ms.unwrap_or(0).max(snapshot.processed_ms),
            created_at: now_secs,
        };
        Some(
            record
                .transcript
                .build(&meta, Some(IN_PROGRESS_DETAIL.to_string())),
        )
    }

    /// Records a pipeline event and returns the events to send for it.
    /// Status changes are sent at once; progress at most every 250 ms.
    fn apply(
        &mut self,
        job_id: u64,
        model_id: &str,
        event: PipelineEvent,
        now: Instant,
    ) -> Vec<FileTranscriptionEvent> {
        let Some(record) = self.record_mut(job_id) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        let snapshot = &mut record.snapshot;
        let immediate = match event {
            PipelineEvent::Probed { duration_ms } => {
                snapshot.duration_ms = duration_ms;
                true
            }
            PipelineEvent::ModelReady => {
                snapshot.model_id = Some(model_id.to_string());
                true
            }
            PipelineEvent::Waiting => set_status(snapshot, FileJobStatus::Waiting),
            PipelineEvent::Speakers(state) => {
                snapshot.speakers = Some(state);
                true
            }
            PipelineEvent::Transcribing { processed_ms, .. } => {
                snapshot.processed_ms = processed_ms;
                set_status(snapshot, FileJobStatus::Transcribing)
            }
            PipelineEvent::Chunk {
                index,
                paragraph,
                segments,
                language,
                processed_ms,
                ..
            } => {
                snapshot.chunks_done += 1;
                snapshot.processed_ms = processed_ms;
                if let Some(paragraph) = paragraph {
                    let grain = pipeline::chunk_grain(&paragraph, &segments);
                    record.transcript.push_chunk(
                        Some(paragraph),
                        segments.clone(),
                        language,
                        grain,
                    );
                    snapshot.has_transcript = true;
                    snapshot.transcript_status = Some(TranscriptStatus::Partial);
                }
                if !segments.is_empty() {
                    events.push(FileTranscriptionEvent::Chunk {
                        job_id,
                        chunk_index: index,
                        segments,
                    });
                }
                false
            }
        };

        let due = record
            .last_progress_emit
            .is_none_or(|last| now.saturating_duration_since(last) >= PROGRESS_EMIT_INTERVAL);
        if immediate || due {
            record.last_progress_emit = Some(now);
            events.push(FileTranscriptionEvent::Updated {
                job: record.snapshot.clone(),
            });
        }
        events
    }

    /// Records how a job ended; returns its final snapshot.
    fn finish(&mut self, job_id: u64, finish: JobFinish) -> Option<FileJobSnapshot> {
        let record = self.record_mut(job_id)?;
        let snapshot = &mut record.snapshot;
        snapshot.status = finish.status;
        snapshot.error = finish.error;
        if finish.decoder.is_some() {
            snapshot.decoder = finish.decoder;
        }
        snapshot.has_transcript = finish.result.is_some();
        snapshot.transcript_status = finish.result.as_ref().map(|t| t.status);
        snapshot.history_id = finish.result.as_ref().and_then(|t| t.history_id);
        if let Some(transcript) = &finish.result {
            if finish.status == FileJobStatus::Completed {
                snapshot.duration_ms = Some(transcript.duration_ms);
                snapshot.processed_ms = transcript.duration_ms;
            }
        }
        record.result = finish.result;
        // The result holds everything the live view had.
        record.transcript = TranscriptBuilder::default();
        Some(snapshot.clone())
    }

    /// Drops the oldest finished jobs beyond `keep`; returns their ids.
    fn evict_finished(&mut self, keep: usize) -> Vec<u64> {
        let finished = self
            .jobs
            .iter()
            .filter(|job| is_finished(job.snapshot.status))
            .count();
        let mut excess = finished.saturating_sub(keep);
        let mut evicted = Vec::new();
        self.jobs.retain(|job| {
            if excess > 0 && is_finished(job.snapshot.status) {
                excess -= 1;
                evicted.push(job.snapshot.job_id);
                false
            } else {
                true
            }
        });
        evicted
    }
}

/// Sets the status; true if it changed.
fn set_status(snapshot: &mut FileJobSnapshot, status: FileJobStatus) -> bool {
    let changed = snapshot.status != status;
    snapshot.status = status;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::types::{
        TimingGrain, TranscriptSegment, TranscriptSpeaker, TranscriptSpeakers,
    };

    const WITH_HISTORY: FileTranscriptionOptions = FileTranscriptionOptions {
        save_to_history: true,
        label_speakers: false,
    };

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment::new(start_ms, end_ms, text)
    }

    fn table_with_queued(count: u64) -> JobTable {
        let mut table = JobTable::default();
        for job_id in 1..=count {
            table.add(
                job_id,
                format!("/audio/file-{job_id}.mp3"),
                WITH_HISTORY,
                FileTranscriptionEngine::Handy,
                Ok(()),
                1_000 + job_id as i64,
            );
        }
        table
    }

    fn status(table: &JobTable, job_id: u64) -> FileJobStatus {
        table.record(job_id).expect("job exists").snapshot.status
    }

    fn finished(status: FileJobStatus, result: Option<FileTranscript>) -> JobFinish {
        JobFinish {
            status,
            error: None,
            decoder: Some(DecoderKind::Symphonia),
            result,
        }
    }

    fn transcript(text: &str) -> FileTranscript {
        FileTranscript {
            source_path: "/audio/file-1.mp3".to_string(),
            source_name: "file-1.mp3".to_string(),
            duration_ms: 42_000,
            model_id: "tiny".to_string(),
            model_name: Some("Tiny".to_string()),
            language: Some("en".to_string()),
            status: TranscriptStatus::Completed,
            status_detail: None,
            timing: TimingGrain::Chunk,
            text: text.to_string(),
            paragraphs: vec![segment(0, 42_000, text)],
            segments: vec![segment(0, 42_000, text)],
            created_at: 1_700_000_000,
            history_id: Some(9),
            speakers: None,
        }
    }

    fn chunk_event(
        index: u32,
        paragraph: Option<TranscriptSegment>,
        processed_ms: u64,
    ) -> PipelineEvent {
        let segments = paragraph.iter().cloned().collect();
        PipelineEvent::Chunk {
            index,
            paragraph,
            segments,
            language: Some("en".to_string()),
            processed_ms,
            skipped: false,
        }
    }

    fn updated_count(events: &[FileTranscriptionEvent]) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, FileTranscriptionEvent::Updated { .. }))
            .count()
    }

    #[test]
    fn validate_path_reports_missing_and_unsupported_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supported = |path: &Path| path.extension().is_some_and(|ext| ext == "mp3");

        let missing =
            validate_path(&dir.path().join("missing.mp3"), supported).expect_err("missing file");
        assert_eq!(missing.code, FileJobErrorCode::NotFound);

        let directory = validate_path(dir.path(), supported).expect_err("directory");
        assert_eq!(directory.code, FileJobErrorCode::NotFound);

        let unknown = dir.path().join("notes.XYZ");
        std::fs::write(&unknown, b"x").expect("write file");
        let unsupported = validate_path(&unknown, supported).expect_err("unsupported");
        assert_eq!(unsupported.code, FileJobErrorCode::UnsupportedFormat);
        assert_eq!(unsupported.message, "Unsupported file type: .xyz");

        let no_extension = dir.path().join("README");
        std::fs::write(&no_extension, b"x").expect("write file");
        assert_eq!(
            validate_path(&no_extension, supported)
                .expect_err("no extension")
                .code,
            FileJobErrorCode::UnsupportedFormat
        );

        let audio = dir.path().join("talk.mp3");
        std::fs::write(&audio, b"x").expect("write file");
        assert!(validate_path(&audio, supported).is_ok());
    }

    #[test]
    fn add_builds_queued_and_failed_snapshots() {
        let mut table = JobTable::default();
        let queued = table.add(
            1,
            "/audio/Talk.MP3".to_string(),
            WITH_HISTORY,
            FileTranscriptionEngine::Handy,
            Ok(()),
            1_234,
        );
        let failed = table.add(
            2,
            "/audio/notes.xyz".to_string(),
            FileTranscriptionOptions::default(),
            FileTranscriptionEngine::Lemonade,
            Err(FileJobError {
                code: FileJobErrorCode::UnsupportedFormat,
                message: "Unsupported file type: .xyz".to_string(),
            }),
            1_235,
        );

        assert_eq!(queued.job_id, 1);
        assert_eq!(queued.path, "/audio/Talk.MP3");
        assert_eq!(queued.file_name, "Talk.MP3");
        assert_eq!(queued.status, FileJobStatus::Queued);
        assert_eq!(queued.enqueued_at, 1_234);
        assert_eq!(queued.processed_ms, 0);
        assert_eq!(queued.chunks_done, 0);
        assert!(queued.error.is_none());
        assert!(!queued.has_transcript);
        assert_eq!(queued.duration_ms, None);
        assert_eq!(queued.model_id, None);
        assert_eq!(queued.history_id, None);

        assert_eq!(failed.status, FileJobStatus::Failed);
        assert_eq!(queued.engine, FileTranscriptionEngine::Handy);
        assert_eq!(failed.engine, FileTranscriptionEngine::Lemonade);
        assert_eq!(
            failed.error.as_ref().map(|error| error.code),
            Some(FileJobErrorCode::UnsupportedFormat)
        );
        assert_eq!(
            table
                .snapshots()
                .iter()
                .map(|job| job.job_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn start_next_is_fifo_and_skips_finished_jobs() {
        let mut table = table_with_queued(3);
        table.cancel(1).expect("cancel queued job");

        let first = table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("job 2 starts");
        assert_eq!(first.job_id, 2);
        assert_eq!(first.path, "/audio/file-2.mp3");
        assert!(first.options.save_to_history);
        assert_eq!(first.snapshot.status, FileJobStatus::Preparing);
        assert_eq!(status(&table, 2), FileJobStatus::Preparing);

        let second = table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("job 3 starts");
        assert_eq!(second.job_id, 3);
        assert!(table.start_next(FileTranscriptionEngine::Handy).is_none());
    }

    #[test]
    fn cancelling_a_queued_job_is_immediate() {
        let mut table = table_with_queued(2);

        match table.cancel(2).expect("cancel") {
            Cancellation::Dequeued(snapshot) => {
                assert_eq!(snapshot.job_id, 2);
                assert_eq!(snapshot.status, FileJobStatus::Cancelled);
            }
            Cancellation::Running(_) => panic!("job 2 was queued"),
        }
        assert_eq!(status(&table, 2), FileJobStatus::Cancelled);
        assert!(table.cancel(2).is_err(), "already finished");
        assert!(table.cancel(99).is_err(), "unknown job");
    }

    #[test]
    fn cancelling_a_running_job_cancels_its_token() {
        let mut table = table_with_queued(1);
        let started = table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");

        match table.cancel(1).expect("cancel") {
            Cancellation::Running(token) => assert!(token.same_as(&started.token)),
            Cancellation::Dequeued(_) => panic!("job 1 was running"),
        }
        assert!(started.token.is_cancelled());
        // The status changes only when the pipeline returns.
        assert_eq!(status(&table, 1), FileJobStatus::Preparing);
        assert!(table.cancel(1).is_ok(), "cancelling again is harmless");
    }

    #[test]
    fn cancel_all_dequeues_and_collects_running_tokens() {
        let mut table = table_with_queued(3);
        let running = table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        table.finish(1, finished(FileJobStatus::Completed, None));
        let next = table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start job 2");
        assert_eq!(next.job_id, 2);
        assert!(!running.token.is_cancelled());

        let (dequeued, tokens) = table.cancel_all();

        assert_eq!(
            dequeued.iter().map(|job| job.job_id).collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(tokens.len(), 1);
        assert!(tokens[0].same_as(&next.token));
        assert!(next.token.is_cancelled());
        assert_eq!(status(&table, 1), FileJobStatus::Completed);
    }

    #[test]
    fn dismiss_only_removes_finished_jobs() {
        let mut table = table_with_queued(2);
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");

        assert!(table.dismiss(1).is_err(), "running");
        assert!(table.dismiss(2).is_err(), "queued");
        assert!(table.dismiss(3).is_err(), "unknown");

        table.finish(1, finished(FileJobStatus::Failed, None));
        table.dismiss(1).expect("dismiss finished job");
        assert!(table.record(1).is_none());
        assert_eq!(table.snapshots().len(), 1);
    }

    #[test]
    fn eviction_drops_the_oldest_finished_jobs() {
        let mut table = table_with_queued(25);
        for _ in 0..23 {
            let job = table
                .start_next(FileTranscriptionEngine::Handy)
                .expect("start");
            table.finish(job.job_id, finished(FileJobStatus::Completed, None));
        }
        // Job 24 is running and job 25 is queued; neither may be evicted.
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start job 24");

        let evicted = table.evict_finished(MAX_FINISHED_JOBS);

        assert_eq!(evicted, vec![1, 2, 3]);
        let remaining: Vec<u64> = table.snapshots().iter().map(|job| job.job_id).collect();
        assert_eq!(remaining, (4..=25).collect::<Vec<_>>());
        assert!(table.evict_finished(MAX_FINISHED_JOBS).is_empty());
    }

    #[test]
    fn events_update_the_snapshot() {
        let mut table = table_with_queued(1);
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        let start = Instant::now();

        let events = table.apply(
            1,
            "tiny",
            PipelineEvent::Probed {
                duration_ms: Some(60_000),
            },
            start,
        );
        assert_eq!(updated_count(&events), 1);
        table.apply(1, "tiny", PipelineEvent::ModelReady, start);
        table.apply(
            1,
            "tiny",
            PipelineEvent::Transcribing {
                chunk_index: 0,
                processed_ms: 0,
            },
            start,
        );
        let events = table.apply(
            1,
            "tiny",
            chunk_event(0, Some(segment(0, 28_000, "Hello.")), 28_000),
            start + Duration::from_secs(1),
        );

        match events.first() {
            Some(FileTranscriptionEvent::Chunk {
                job_id,
                chunk_index,
                segments,
            }) => {
                assert_eq!((*job_id, *chunk_index), (1, 0));
                assert_eq!(segments, &vec![segment(0, 28_000, "Hello.")]);
            }
            other => panic!("expected a Chunk event first, got {other:?}"),
        }
        let snapshot = &table.record(1).expect("job").snapshot;
        assert_eq!(snapshot.status, FileJobStatus::Transcribing);
        assert_eq!(snapshot.duration_ms, Some(60_000));
        assert_eq!(snapshot.model_id.as_deref(), Some("tiny"));
        assert_eq!(snapshot.processed_ms, 28_000);
        assert_eq!(snapshot.chunks_done, 1);
        assert!(snapshot.has_transcript);
        assert_eq!(snapshot.transcript_status, Some(TranscriptStatus::Partial));
    }

    #[test]
    fn progress_updates_are_throttled_but_status_changes_are_not() {
        let mut table = table_with_queued(1);
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        let start = Instant::now();
        let transcribing = |processed_ms| PipelineEvent::Transcribing {
            chunk_index: 0,
            processed_ms,
        };

        assert_eq!(
            updated_count(&table.apply(1, "tiny", transcribing(0), start)),
            1
        );
        // Same status, 100 ms later: throttled.
        let at_100ms = start + Duration::from_millis(100);
        assert_eq!(
            updated_count(&table.apply(1, "tiny", chunk_event(0, None, 1_000), at_100ms)),
            0
        );
        // A status change goes out at once.
        assert_eq!(
            updated_count(&table.apply(1, "tiny", PipelineEvent::Waiting, at_100ms)),
            1
        );
        assert_eq!(
            updated_count(&table.apply(1, "tiny", transcribing(1_000), at_100ms)),
            1
        );
        // Progress again once 250 ms have passed.
        let at_200ms = start + Duration::from_millis(200);
        assert_eq!(
            updated_count(&table.apply(1, "tiny", chunk_event(1, None, 2_000), at_200ms)),
            0
        );
        let at_400ms = start + Duration::from_millis(400);
        let events = table.apply(1, "tiny", chunk_event(2, None, 3_000), at_400ms);
        assert_eq!(updated_count(&events), 1);
        // Chunks without segments send no Chunk event.
        assert_eq!(events.len(), 1);
        assert_eq!(table.record(1).expect("job").snapshot.chunks_done, 3);
    }

    #[test]
    fn speaker_state_starts_pending_only_when_requested_and_is_sent_at_once() {
        let mut table = table_with_queued(1);
        let labelled = FileTranscriptionOptions {
            save_to_history: true,
            label_speakers: true,
        };
        let snapshot = table.add(
            2,
            "/audio/talk.mp3".to_string(),
            labelled,
            FileTranscriptionEngine::Handy,
            Ok(()),
            2_000,
        );
        assert_eq!(snapshot.speakers, Some(SpeakerJobState::Pending));
        assert_eq!(snapshot.engine, FileTranscriptionEngine::Handy);
        assert_eq!(table.record(1).expect("job").snapshot.speakers, None);

        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        let now = Instant::now();
        table.apply(1, "tiny", PipelineEvent::ModelReady, now);
        // Within the progress throttle window, but a state change is never held back.
        let events = table.apply(
            1,
            "tiny",
            PipelineEvent::Speakers(SpeakerJobState::Running),
            now,
        );
        assert_eq!(updated_count(&events), 1);
        assert_eq!(
            table.record(1).expect("job").snapshot.speakers,
            Some(SpeakerJobState::Running)
        );
    }

    #[test]
    fn events_for_unknown_jobs_are_ignored() {
        let mut table = table_with_queued(1);
        assert!(table
            .apply(7, "tiny", PipelineEvent::ModelReady, Instant::now())
            .is_empty());
    }

    #[test]
    fn running_transcript_is_partial_and_in_progress() {
        let mut table = table_with_queued(1);
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        let now = Instant::now();
        assert!(table.transcript(1, 5).is_none(), "nothing transcribed yet");

        table.apply(
            1,
            "tiny",
            PipelineEvent::Probed {
                duration_ms: Some(90_000),
            },
            now,
        );
        table.apply(1, "tiny", PipelineEvent::ModelReady, now);
        table.apply(
            1,
            "tiny",
            chunk_event(0, Some(segment(0, 28_000, "One.")), 28_000),
            now,
        );
        table.apply(1, "tiny", chunk_event(1, None, 56_000), now);
        let mut event = chunk_event(2, Some(segment(56_000, 80_000, "Two.")), 80_000);
        if let PipelineEvent::Chunk { segments, .. } = &mut event {
            *segments = vec![segment(57_000, 60_000, "Two.")];
        }
        table.apply(1, "tiny", event, now);

        let transcript = table.transcript(1, 5).expect("running transcript");
        assert_eq!(transcript.status, TranscriptStatus::Partial);
        assert_eq!(
            transcript.status_detail.as_deref(),
            Some(IN_PROGRESS_DETAIL)
        );
        assert_eq!(transcript.text, "One.\n\nTwo.");
        assert_eq!(transcript.segments.len(), 2);
        // The first chunk had only chunk-level timing.
        assert_eq!(transcript.timing, TimingGrain::Chunk);
        assert_eq!(transcript.duration_ms, 90_000);
        assert_eq!(transcript.model_id, "tiny");
        assert_eq!(transcript.source_name, "file-1.mp3");
        assert_eq!(transcript.language.as_deref(), Some("en"));
        assert_eq!(transcript.created_at, 5);
        assert!(table.transcript(42, 5).is_none(), "unknown job");
    }

    #[test]
    fn finish_records_the_result() {
        let mut table = table_with_queued(1);
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        table.apply(
            1,
            "tiny",
            chunk_event(0, Some(segment(0, 28_000, "Hi.")), 28_000),
            Instant::now(),
        );

        let snapshot = table
            .finish(
                1,
                finished(FileJobStatus::Completed, Some(transcript("Hi."))),
            )
            .expect("job exists");

        assert_eq!(snapshot.status, FileJobStatus::Completed);
        assert_eq!(snapshot.decoder, Some(DecoderKind::Symphonia));
        assert!(snapshot.has_transcript);
        assert_eq!(
            snapshot.transcript_status,
            Some(TranscriptStatus::Completed)
        );
        assert_eq!(snapshot.history_id, Some(9));
        assert_eq!(snapshot.duration_ms, Some(42_000));
        assert_eq!(snapshot.processed_ms, 42_000);
        assert_eq!(table.transcript(1, 0), Some(transcript("Hi.")));
        assert!(!table.record(1).expect("job").transcript.has_text());
    }

    #[test]
    fn failed_job_without_result_has_no_transcript() {
        let mut table = table_with_queued(1);
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        table.apply(
            1,
            "tiny",
            chunk_event(0, Some(segment(0, 28_000, "Hi.")), 28_000),
            Instant::now(),
        );

        let snapshot = table
            .finish(
                1,
                JobFinish {
                    status: FileJobStatus::Failed,
                    error: Some(FileJobError {
                        code: FileJobErrorCode::ModelChanged,
                        message: "changed".to_string(),
                    }),
                    decoder: None,
                    result: None,
                },
            )
            .expect("job exists");

        assert_eq!(snapshot.status, FileJobStatus::Failed);
        assert_eq!(
            snapshot.error.map(|error| error.code),
            Some(FileJobErrorCode::ModelChanged)
        );
        assert!(!snapshot.has_transcript);
        assert_eq!(snapshot.transcript_status, None);
        assert_eq!(snapshot.decoder, None);
        assert!(table.transcript(1, 0).is_none());
    }

    #[test]
    fn the_engine_is_fixed_when_the_job_starts() {
        let mut table = table_with_queued(2);
        assert_eq!(
            table.record(1).expect("job").snapshot.engine,
            FileTranscriptionEngine::Handy
        );

        let started = table
            .start_next(FileTranscriptionEngine::Lemonade)
            .expect("start");

        assert_eq!(started.snapshot.engine, FileTranscriptionEngine::Lemonade);
        assert_eq!(
            table.record(1).expect("job").snapshot.engine,
            FileTranscriptionEngine::Lemonade
        );
        // Still queued: start_next leaves it to refresh_queued_engine.
        assert_eq!(
            table.record(2).expect("job").snapshot.engine,
            FileTranscriptionEngine::Handy
        );
    }

    #[test]
    fn queued_jobs_show_the_engine_selected_now() {
        let mut table = table_with_queued(3);
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        table.cancel(3).expect("cancel");

        // The engine setting changes while job 2 waits: it starts on the new
        // engine, so it shows it.
        let refreshed = table.refresh_queued_engine(FileTranscriptionEngine::Lemonade);
        assert_eq!(
            refreshed.iter().map(|job| job.job_id).collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(refreshed[0].engine, FileTranscriptionEngine::Lemonade);
        let engine = |job_id| table.record(job_id).expect("job").snapshot.engine;
        assert_eq!(engine(1), FileTranscriptionEngine::Handy, "running");
        assert_eq!(engine(2), FileTranscriptionEngine::Lemonade);
        assert_eq!(engine(3), FileTranscriptionEngine::Handy, "cancelled");

        // Nothing changed, nothing to send.
        assert!(table
            .refresh_queued_engine(FileTranscriptionEngine::Lemonade)
            .is_empty());
    }

    #[test]
    fn job_engine_follows_the_settings() {
        let mut settings = crate::settings::get_default_settings();
        settings.selected_model = "small".to_string();
        settings.lemonade.model = " Whisper-Base ".to_string();
        settings.lemonade.api_key = "  ".to_string();

        let (engine, model_id) = job_engine(&settings);
        assert_eq!(model_id, "small");
        assert!(matches!(
            engine,
            PipelineEngine::Handy { model_id, device_index: None } if model_id == "small"
        ));

        settings.file_transcription_engine = FileTranscriptionEngine::Lemonade;
        let (engine, model_id) = job_engine(&settings);
        assert_eq!(model_id, "lemonade:Whisper-Base");
        match engine {
            PipelineEngine::Lemonade { config, settings } => {
                assert_eq!(config.model, "Whisper-Base");
                assert_eq!(config.base_url, "http://localhost:13305");
                assert_eq!(config.api_key, None);
                assert_eq!(settings.selected_model, "small");
            }
            PipelineEngine::Handy { .. } => panic!("expected the Lemonade engine"),
        }
    }

    #[test]
    fn edit_job_transcript_changes_only_finished_results() {
        let mut table = table_with_queued(3);
        let rename = |t: &mut FileTranscript| {
            t.text = "Edited.".to_string();
            Ok(())
        };
        assert!(table.edit_result(42, rename).is_err(), "unknown job");
        assert!(table.edit_result(1, rename).is_err(), "queued");
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        let err = table.edit_result(1, rename).unwrap_err();
        assert!(err.contains("still running"), "{err}");

        table.finish(
            1,
            finished(FileJobStatus::Completed, Some(transcript("Hi."))),
        );
        let edited = table.edit_result(1, rename).expect("edit");
        assert_eq!(edited.text, "Edited.");
        assert_eq!(table.transcript(1, 0).expect("result").text, "Edited.");

        // A failed edit keeps what was stored.
        let err = table
            .edit_result(1, |t| {
                t.text = "Half done".to_string();
                Err("no such speaker".to_string())
            })
            .unwrap_err();
        assert_eq!(err, "no such speaker");
        assert_eq!(table.transcript(1, 0).expect("result").text, "Edited.");

        // A finished job without a result has nothing to edit.
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        table.finish(2, finished(FileJobStatus::Cancelled, None));
        let err = table.edit_result(2, rename).unwrap_err();
        assert!(err.contains("no transcript"), "{err}");
    }

    #[test]
    fn job_history_id_is_read_from_finished_results() {
        let mut table = table_with_queued(2);
        assert!(table.result_mut(42).is_err(), "unknown job");
        assert!(table.result_mut(1).is_err(), "queued");
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");
        table.finish(
            1,
            finished(FileJobStatus::Completed, Some(transcript("Hi."))),
        );
        assert_eq!(table.result_mut(1).map(|t| t.history_id), Ok(Some(9)));
    }

    #[test]
    fn a_history_rename_reaches_the_job_saved_as_that_entry() {
        let mut table = table_with_queued(3);
        for job_id in 1..=2 {
            table
                .start_next(FileTranscriptionEngine::Handy)
                .expect("start");
            let mut result = transcript("Speaker 1: Hi.");
            result.history_id = Some(8 + job_id as i64);
            table.finish(job_id, finished(FileJobStatus::Completed, Some(result)));
        }
        table
            .start_next(FileTranscriptionEngine::Handy)
            .expect("start");

        let stored = FileTranscript {
            text: "Ann: Hi.".to_string(),
            speakers: Some(TranscriptSpeakers {
                speakers: vec![TranscriptSpeaker {
                    id: 1,
                    name: Some("Ann".to_string()),
                }],
                approximate: true,
                unassigned_segments: 0,
                model: "test".to_string(),
            }),
            ..transcript("Ann: Hi.")
        };
        let synced = table.sync_history_speakers(10, &stored);

        assert_eq!(synced.iter().map(|job| job.job_id).collect::<Vec<_>>(), [2]);
        let result = table.transcript(2, 0).expect("result");
        assert_eq!(result.text, "Ann: Hi.");
        assert_eq!(result.speakers, stored.speakers);
        // Other entries' jobs and running jobs keep what they have.
        let other = table.transcript(1, 0).expect("result");
        assert_eq!(other.text, "Speaker 1: Hi.");
        assert_eq!(other.speakers, None);
        assert!(table.record(3).expect("job").result.is_none());
    }
}
