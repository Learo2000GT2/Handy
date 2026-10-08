//! The GUI's file transcription queue: one worker thread runs the jobs one
//! at a time, in the order they were added, publishes their state as
//! `FileTranscriptionEvent`s and saves results to history.
//!
//! Jobs live only in memory. The window hides on close, so jobs keep running
//! until the app exits; only what was saved to history outlives the app.

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::formats;
use crate::file_transcription::pipeline::{
    self, PipelineError, PipelineEvent, PipelineOptions, PipelineOutput, TranscriptBuilder,
    TranscriptMeta,
};
use crate::file_transcription::types::{
    DecoderKind, FileJobError, FileJobErrorCode, FileJobSnapshot, FileJobStatus, FileTranscript,
    FileTranscriptionEvent, FileTranscriptionOptions, TranscriptStatus,
};
use crate::managers::history::HistoryManager;
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
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

        let (snapshots, evicted) = {
            let mut state = self.lock_state();
            let snapshots: Vec<FileJobSnapshot> = checked
                .into_iter()
                .map(|(path, validation)| {
                    let job_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    state.add(job_id, path, options.clone(), validation, enqueued_at)
                })
                .collect();
            (snapshots, state.evict_finished(MAX_FINISHED_JOBS))
        };
        self.wake.notify_one();

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

    /// Enqueue order.
    pub fn list(&self) -> Vec<FileJobSnapshot> {
        self.lock_state().snapshots()
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
            let job = {
                let mut state = self.lock_state();
                loop {
                    if let Some(job) = state.start_next() {
                        break job;
                    }
                    state = self
                        .wake
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            self.emit(FileTranscriptionEvent::Updated {
                job: job.snapshot.clone(),
            });

            self.run_job(&job);

            crate::memory::trim_freed_memory();
            let evicted = self.lock_state().evict_finished(MAX_FINISHED_JOBS);
            self.emit_removed(evicted);
        }
    }

    /// Runs one job to its end. The job table is locked only briefly to
    /// record progress: never while the pipeline works or an event is sent.
    fn run_job(&self, job: &StartedJob) {
        // The model is fixed when the job starts, not when it was queued.
        let model_id = crate::settings::get_settings(&self.app).selected_model;
        let options = PipelineOptions {
            model_id: model_id.clone(),
            device_index: None,
            max_chunk_ms: None,
            ffmpeg: None,
            skip_silent_chunks: true,
        };
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
        transcript.model_name = self
            .app
            .try_state::<Arc<ModelManager>>()
            .and_then(|models| models.get_model_info(model_id))
            .map(|info| info.name);
        transcript.created_at = Utc::now().timestamp();
        transcript
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
        validation: Result<(), FileJobError>,
        enqueued_at: i64,
    ) -> FileJobSnapshot {
        let error = validation.err();
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

    /// Takes the oldest queued job and marks it `Preparing`.
    fn start_next(&mut self) -> Option<StartedJob> {
        let record = self
            .jobs
            .iter_mut()
            .find(|job| job.snapshot.status == FileJobStatus::Queued)?;
        record.snapshot.status = FileJobStatus::Preparing;
        record.last_progress_emit = None;
        Some(StartedJob {
            job_id: record.snapshot.job_id,
            path: record.snapshot.path.clone(),
            options: record.options.clone(),
            token: record.token.clone(),
            snapshot: record.snapshot.clone(),
        })
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
    use crate::file_transcription::types::{TimingGrain, TranscriptSegment};

    const WITH_HISTORY: FileTranscriptionOptions = FileTranscriptionOptions {
        save_to_history: true,
    };

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    fn table_with_queued(count: u64) -> JobTable {
        let mut table = JobTable::default();
        for job_id in 1..=count {
            table.add(
                job_id,
                format!("/audio/file-{job_id}.mp3"),
                WITH_HISTORY,
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
            Ok(()),
            1_234,
        );
        let failed = table.add(
            2,
            "/audio/notes.xyz".to_string(),
            FileTranscriptionOptions::default(),
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

        let first = table.start_next().expect("job 2 starts");
        assert_eq!(first.job_id, 2);
        assert_eq!(first.path, "/audio/file-2.mp3");
        assert!(first.options.save_to_history);
        assert_eq!(first.snapshot.status, FileJobStatus::Preparing);
        assert_eq!(status(&table, 2), FileJobStatus::Preparing);

        let second = table.start_next().expect("job 3 starts");
        assert_eq!(second.job_id, 3);
        assert!(table.start_next().is_none());
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
        let started = table.start_next().expect("start");

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
        let running = table.start_next().expect("start");
        table.finish(1, finished(FileJobStatus::Completed, None));
        let next = table.start_next().expect("start job 2");
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
        table.start_next().expect("start");

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
            let job = table.start_next().expect("start");
            table.finish(job.job_id, finished(FileJobStatus::Completed, None));
        }
        // Job 24 is running and job 25 is queued; neither may be evicted.
        table.start_next().expect("start job 24");

        let evicted = table.evict_finished(MAX_FINISHED_JOBS);

        assert_eq!(evicted, vec![1, 2, 3]);
        let remaining: Vec<u64> = table.snapshots().iter().map(|job| job.job_id).collect();
        assert_eq!(remaining, (4..=25).collect::<Vec<_>>());
        assert!(table.evict_finished(MAX_FINISHED_JOBS).is_empty());
    }

    #[test]
    fn events_update_the_snapshot() {
        let mut table = table_with_queued(1);
        table.start_next().expect("start");
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
        table.start_next().expect("start");
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
    fn events_for_unknown_jobs_are_ignored() {
        let mut table = table_with_queued(1);
        assert!(table
            .apply(7, "tiny", PipelineEvent::ModelReady, Instant::now())
            .is_empty());
    }

    #[test]
    fn running_transcript_is_partial_and_in_progress() {
        let mut table = table_with_queued(1);
        table.start_next().expect("start");
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
        table.start_next().expect("start");
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
        table.start_next().expect("start");
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
}
