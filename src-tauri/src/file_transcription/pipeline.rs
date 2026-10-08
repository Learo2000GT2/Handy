//! The decode → chunk → transcribe pipeline shared by the GUI job manager and
//! the headless CLI.
//!
//! A `file-decode` thread decodes the file to 16 kHz mono and splits it into
//! silence-aware chunks, which reach the calling thread through a small
//! bounded channel, so memory stays flat whatever the file length. The
//! calling thread sends one chunk at a time to the engine (Handy's model or
//! Lemonade Server, see `engine`); with Handy's model it waits while a
//! dictation is active.
//!
//! When speaker labels are asked for, the diarizer runs alongside on its own
//! thread and worker (see `diarize`); its turns are merged onto the finished
//! transcript. A labelling failure never fails the transcription.

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::chunker::{
    AudioChunk, ChunkVerdict, SilenceChunker, SpeechGate, SAMPLES_PER_MS,
};
use crate::file_transcription::decode::{
    self, AudioProbe, DecodeError, DecodeOptions, DecodeOutcome,
};
use crate::file_transcription::diarize::{self, DiarizationHandle, DiarizeError, DiarizeRequest};
use crate::file_transcription::engine::{ChunkTranscriber, TranscribeError};
use crate::file_transcription::export::format_clock;
use crate::file_transcription::formats::DecodeRoute;
use crate::file_transcription::lemonade::{LemonadeJobConfig, LemonadeTranscriber};
use crate::file_transcription::speakers::{self, MergeOptions};
use crate::file_transcription::types::*;
use crate::managers::transcription::{ChunkLimits, ChunkTranscript, TranscriptionManager};
use crate::settings::AppSettings;
use anyhow::anyhow;
use log::{debug, warn};
use std::cell::Cell;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Shortest chunk the chunker accepts (0 means "never split").
const MIN_CHUNK_MS: u32 = 5_000;
/// Decoded chunks buffered ahead of the engine (each about 1.8 MB).
const CHUNK_QUEUE_LEN: usize = 2;
/// How often a waiting job re-checks whether dictation has finished.
const DICTATION_POLL_INTERVAL: Duration = Duration::from_millis(150);
/// Engine attempts per chunk before the chunk is given up.
const ATTEMPTS_PER_CHUNK: u32 = 2;
/// Chunks in a row that may fail before the whole job is abandoned.
const MAX_CONSECUTIVE_FAILED_CHUNKS: u32 = 3;

/// The engine a job transcribes with.
#[derive(Clone, Debug)]
pub enum PipelineEngine {
    /// Handy's model through TranscriptionManager (today's behaviour).
    Handy {
        model_id: String,
        device_index: Option<usize>,
    },
    /// Lemonade Server; never takes the lease, the run gate or the loaded model.
    Lemonade {
        config: LemonadeJobConfig,
        settings: AppSettings,
    },
}

#[derive(Clone, Debug)]
pub struct PipelineOptions {
    pub engine: PipelineEngine,
    /// None = the engine's limits().max_chunk_ms. Some(0) = whole file in one
    /// engine call. Some(n): n >= 5000, used as-is (callers warn if > hard_max_ms).
    pub max_chunk_ms: Option<u32>,
    pub ffmpeg: Option<PathBuf>,
    pub skip_silent_chunks: bool,
    /// Some: label speakers with this Sortformer GGUF (must exist).
    pub speaker_model: Option<PathBuf>,
}

#[derive(Debug)]
pub enum PipelineEvent {
    Probed {
        duration_ms: Option<u64>,
    },
    ModelReady,
    /// Dictation is active; the next chunk waits.
    Waiting,
    /// A chunk is being sent to the engine; processed_ms = its start.
    Transcribing {
        chunk_index: u32,
        processed_ms: u64,
    },
    Chunk {
        index: u32,
        paragraph: Option<TranscriptSegment>,
        segments: Vec<TranscriptSegment>,
        language: Option<String>,
        processed_ms: u64,
        skipped: bool,
    },
    /// Speaker labelling changed state (only when labels were requested).
    Speakers(SpeakerJobState),
}

#[derive(Debug)]
pub struct PipelineOutput {
    /// history_id = None, model_name = None (callers fill these).
    pub transcript: FileTranscript,
    pub decoder: DecoderKind,
    pub chunks: u32,
    pub skipped_chunks: u32,
    pub failed_chunks: u32,
    pub warnings: Vec<String>,
    pub source_size_bytes: u64,
    pub source_modified_at: Option<i64>,
    /// Final speaker state when labels were requested (Done/Failed/Skipped).
    pub speakers: Option<SpeakerJobState>,
}

#[derive(Debug)]
pub enum PipelineError {
    Cancelled {
        partial: Option<Box<PipelineOutput>>,
    },
    Failed {
        code: FileJobErrorCode,
        message: String,
        partial: Option<Box<PipelineOutput>>,
    },
}

impl PipelineError {
    fn failed(code: FileJobErrorCode, message: impl Into<String>) -> Self {
        Self::Failed {
            code,
            message: message.into(),
            partial: None,
        }
    }

    /// 2 for NotFound | UnsupportedFormat | NeedsFfmpeg | DecodeFailed | NoModel |
    /// ServerUnauthorized | ServerModelMissing; 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Failed {
                code:
                    FileJobErrorCode::NotFound
                    | FileJobErrorCode::UnsupportedFormat
                    | FileJobErrorCode::NeedsFfmpeg
                    | FileJobErrorCode::DecodeFailed
                    | FileJobErrorCode::NoModel
                    | FileJobErrorCode::ServerUnauthorized
                    | FileJobErrorCode::ServerModelMissing,
                ..
            } => 2,
            _ => 1,
        }
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { .. } => f.write_str("Transcription was cancelled"),
            Self::Failed { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for PipelineError {}

pub fn run_pipeline(
    tm: &TranscriptionManager,
    path: &Path,
    opts: &PipelineOptions,
    token: &WorkToken,
    on_event: &mut dyn FnMut(PipelineEvent),
) -> Result<PipelineOutput, PipelineError> {
    let decode_options = DecodeOptions {
        ffmpeg: opts.ffmpeg.clone(),
    };
    let probe = decode::probe(path, &decode_options).map_err(|e| decode_failure(e, None))?;
    on_event(PipelineEvent::Probed {
        duration_ms: probe.duration_ms,
    });

    check_engine_config(&opts.engine)?;
    if token.is_cancelled() {
        return Err(PipelineError::Cancelled { partial: None });
    }

    // The diarizer decodes the file on its own, so it starts before the
    // model loads and runs alongside the transcription.
    let mut labels = SpeakerLabels::start(
        opts.speaker_model.as_deref(),
        path,
        &decode_options,
        probe.duration_ms,
        token,
        {
            let tm = tm.clone();
            Arc::new(move || tm.dictation_active())
        },
        on_event,
    );
    let result = transcribe_file(tm, path, opts, &decode_options, &probe, token, on_event);
    labels.finish(result, on_event)
}

/// The usage errors of an engine choice that need no engine to find.
fn check_engine_config(engine: &PipelineEngine) -> Result<(), PipelineError> {
    match engine {
        PipelineEngine::Handy { model_id, .. } if model_id.is_empty() => {
            Err(PipelineError::failed(
                FileJobErrorCode::NoModel,
                "No transcription model is selected",
            ))
        }
        PipelineEngine::Lemonade { config, .. } if config.model.is_empty() => Err(
            PipelineError::failed(FileJobErrorCode::NoModel, "No Lemonade model is selected"),
        ),
        _ => Ok(()),
    }
}

/// Prepares the job's engine and transcribes the file with it. The engine
/// (and with it a Handy model lease) is released when this returns.
fn transcribe_file(
    tm: &TranscriptionManager,
    path: &Path,
    opts: &PipelineOptions,
    decode_options: &DecodeOptions,
    probe: &AudioProbe,
    token: &WorkToken,
    on_event: &mut dyn FnMut(PipelineEvent),
) -> Result<PipelineOutput, PipelineError> {
    let engine: Box<dyn ChunkTranscriber> = match &opts.engine {
        PipelineEngine::Handy {
            model_id,
            device_index,
        } => {
            let lease = tm
                .begin_file_job(model_id, *device_index, token.clone())
                .map_err(|e| {
                    if token.is_cancelled() {
                        PipelineError::Cancelled { partial: None }
                    } else {
                        PipelineError::failed(
                            model_load_error_code(&e),
                            format!("Could not load model {}: {:#}", model_id, e),
                        )
                    }
                })?;
            Box::new(lease)
        }
        PipelineEngine::Lemonade { config, settings } => Box::new(
            LemonadeTranscriber::begin(config, settings.clone(), token.clone())
                .map_err(engine_start_error)?,
        ),
    };
    on_event(PipelineEvent::ModelReady);

    let dictation_active = || tm.dictation_active();
    run_with(
        engine.as_ref(),
        &dictation_active,
        path,
        decode_options,
        probe,
        opts,
        token,
        on_event,
    )
}

/// The pipeline error for an engine that could not start.
pub(crate) fn engine_start_error(error: TranscribeError) -> PipelineError {
    match error {
        TranscribeError::Cancelled => PipelineError::Cancelled { partial: None },
        TranscribeError::Stop { code, message } => PipelineError::failed(code, message),
        TranscribeError::Failed(error) => {
            PipelineError::failed(FileJobErrorCode::EngineFailed, format!("{:#}", error))
        }
        TranscribeError::Yield => PipelineError::failed(
            FileJobErrorCode::Internal,
            "The engine asked to wait before it started",
        ),
    }
}

/// Decodes `path` and transcribes it with `engine`, chunk by chunk.
/// `dictation_active` is polled before each chunk of an engine that yields
/// to dictation.
#[allow(clippy::too_many_arguments)]
fn run_with(
    engine: &dyn ChunkTranscriber,
    dictation_active: &dyn Fn() -> bool,
    path: &Path,
    decode_options: &DecodeOptions,
    probe: &AudioProbe,
    opts: &PipelineOptions,
    token: &WorkToken,
    on_event: &mut dyn FnMut(PipelineEvent),
) -> Result<PipelineOutput, PipelineError> {
    let (max_chunk_ms, search_window_ms) = chunk_plan(opts.max_chunk_ms, engine.limits());
    let mut gate = SpeechGate::new(opts.skip_silent_chunks).map_err(|e| {
        PipelineError::failed(
            FileJobErrorCode::Internal,
            format!("Could not start the speech detector: {:#}", e),
        )
    })?;
    debug!(
        "Transcribing {} with {} in chunks of at most {} ms",
        path.display(),
        engine.model_id(),
        max_chunk_ms
    );

    let job = JobInfo {
        source_path: source_path_string(path),
        source_name: source_display_name(path),
        model_id: engine.model_id(),
        estimated_duration_ms: probe.duration_ms,
        fallback_decoder: route_decoder(probe.route),
        source_size_bytes: probe.source_size_bytes,
        source_modified_at: probe.source_modified_at,
    };
    let engine = ChunkEngine {
        engine,
        dictation_active,
        token,
    };
    let mut run = ChunkRun::default();
    let (tx, rx) = mpsc::sync_channel::<AudioChunk>(CHUNK_QUEUE_LEN);

    thread::scope(|scope| {
        let producer = thread::Builder::new()
            .name("file-decode".to_string())
            .spawn_scoped(scope, move || {
                decode_into_chunks(
                    path,
                    decode_options,
                    token,
                    max_chunk_ms,
                    search_window_ms,
                    tx,
                )
            })
            .map_err(|e| {
                PipelineError::failed(
                    FileJobErrorCode::Internal,
                    format!("Could not start the decoder thread: {}", e),
                )
            })?;

        let stop = transcribe_chunks(&engine, &rx, &mut gate, &mut run, on_event);
        // Leaving the loop early must stop the decoder: its next send fails.
        drop(rx);
        let produced = producer.join();

        if let Some(stop) = stop {
            let reason = stop.reason(run.processed_ms);
            return Err(stop.into_error(run.partial(&job, None, reason)));
        }
        let Ok((decoded, finished)) = produced else {
            let message = "The decoder thread crashed".to_string();
            return Err(PipelineError::Failed {
                code: FileJobErrorCode::Internal,
                partial: run.partial(&job, None, message.clone()),
                message,
            });
        };
        let outcome = match decoded {
            Ok(outcome) => outcome,
            Err(e) if run.chunks == 0 => return Err(decode_failure(e, None)),
            Err(e) => {
                let reason = match &e {
                    DecodeError::Cancelled => cancelled_reason(run.processed_ms),
                    e => format!("Decoding failed: {}", e),
                };
                return Err(decode_failure(e, run.partial(&job, None, reason)));
            }
        };
        if let Err(e) = finished {
            let message = format!("Could not split the audio into chunks: {:#}", e);
            return Err(PipelineError::Failed {
                code: FileJobErrorCode::Internal,
                partial: run.partial(&job, Some(&outcome), message.clone()),
                message,
            });
        }
        if let Some(message) = run.engine_failure() {
            // No partial: there is no text to keep.
            return Err(PipelineError::failed(
                FileJobErrorCode::EngineFailed,
                message,
            ));
        }
        Ok(run.finish(&job, &outcome))
    })
}

/// Speaker labelling of one job: the diarization running alongside the
/// transcription, if labels were asked for. Dropping it stops the diarizer.
struct SpeakerLabels {
    handle: Option<DiarizationHandle>,
    /// The job's token: a cancel while waiting for labels cancels the job.
    job: WorkToken,
}

impl SpeakerLabels {
    /// Starts diarizing with `model` (Some when labels were asked for) and
    /// reports `Speakers(Running)`. `hold` tells the diarizer to wait before
    /// it starts its worker (a dictation is active).
    fn start(
        model: Option<&Path>,
        path: &Path,
        decode_options: &DecodeOptions,
        estimated_duration_ms: Option<u64>,
        token: &WorkToken,
        hold: Arc<dyn Fn() -> bool + Send + Sync>,
        on_event: &mut dyn FnMut(PipelineEvent),
    ) -> Self {
        let handle = model.map(|model_path| {
            let request = DiarizeRequest {
                path: path.to_path_buf(),
                decode: decode_options.clone(),
                model_path: model_path.to_path_buf(),
                estimated_duration_ms,
            };
            let handle = diarize::start_diarization(request, token.clone(), hold);
            on_event(PipelineEvent::Speakers(SpeakerJobState::Running));
            handle
        });
        Self {
            handle,
            job: token.clone(),
        }
    }

    /// Completes the job: waits for the diarizer and labels a finished
    /// transcript, reporting the final speaker state. A labelling failure
    /// keeps the transcript, without labels, and its status. A job cancelled
    /// while waiting for labels ends cancelled, with its whole transcript as
    /// the partial (kept for viewing, never saved to History). A cancelled or
    /// failed transcription stops the diarizer: partial transcripts are never
    /// labelled.
    fn finish(
        &mut self,
        result: Result<PipelineOutput, PipelineError>,
        on_event: &mut dyn FnMut(PipelineEvent),
    ) -> Result<PipelineOutput, PipelineError> {
        let Some(handle) = self.handle.take() else {
            return result;
        };
        match result {
            Ok(mut output) => {
                let state = label_speakers(handle, &mut output);
                on_event(PipelineEvent::Speakers(state.clone()));
                output.speakers = Some(state);
                if self.job.is_cancelled() {
                    return Err(PipelineError::Cancelled {
                        partial: Some(Box::new(output)),
                    });
                }
                Ok(output)
            }
            Err(mut error) => {
                handle.cancel();
                let state = match &error {
                    PipelineError::Cancelled { .. } => DiarizeError::Cancelled.job_state(),
                    PipelineError::Failed { .. } => SpeakerJobState::Failed {
                        message: "the transcription failed".to_string(),
                    },
                };
                on_event(PipelineEvent::Speakers(state.clone()));
                if let PipelineError::Cancelled {
                    partial: Some(partial),
                }
                | PipelineError::Failed {
                    partial: Some(partial),
                    ..
                } = &mut error
                {
                    partial.speakers = Some(state);
                }
                Err(error)
            }
        }
    }
}

impl Drop for SpeakerLabels {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
    }
}

/// Waits for the diarizer and merges its turns onto `output`'s transcript.
/// Returns the final speaker state.
fn label_speakers(handle: DiarizationHandle, output: &mut PipelineOutput) -> SpeakerJobState {
    if output.transcript.paragraphs.is_empty() {
        // Nothing to label. The job is over, so cancelling its token only
        // stops the diarizer.
        handle.cancel();
        return SpeakerJobState::Skipped {
            reason: SpeakerSkipReason::NoSpeech,
        };
    }
    match handle.wait() {
        Ok(turns) => {
            let options = MergeOptions::for_transcript(&output.transcript);
            let stats = speakers::apply_speakers(
                &mut output.transcript,
                &turns,
                diarize::DIARIZER_MODEL_ID,
                &options,
            );
            debug!(
                "Speaker labels: {} speaker(s), {} unassigned, {} split segment(s)",
                stats.speakers, stats.unassigned, stats.split
            );
            if stats.speakers == 0 {
                SpeakerJobState::Skipped {
                    reason: SpeakerSkipReason::NoSpeech,
                }
            } else {
                SpeakerJobState::Done {
                    speakers: stats.speakers,
                }
            }
        }
        Err(DiarizeError::Cancelled) => DiarizeError::Cancelled.job_state(),
        Err(error) => {
            warn!("Speaker labels failed: {}", error);
            output.warnings.push(format!("Speaker labels: {}", error));
            error.job_state()
        }
    }
}

/// Facts about the job that every result (final or partial) needs.
struct JobInfo<'a> {
    source_path: String,
    source_name: String,
    model_id: &'a str,
    estimated_duration_ms: Option<u64>,
    /// Reported for partial results, where decoding never said which decoder ran.
    fallback_decoder: DecoderKind,
    source_size_bytes: u64,
    source_modified_at: Option<i64>,
}

/// What the chunk loop needs to reach the engine.
struct ChunkEngine<'a> {
    engine: &'a dyn ChunkTranscriber,
    dictation_active: &'a dyn Fn() -> bool,
    token: &'a WorkToken,
}

/// Why the chunk loop stopped before the decoder ran dry.
#[derive(Debug)]
enum Stop {
    Cancelled,
    Failed {
        code: FileJobErrorCode,
        message: String,
    },
}

impl Stop {
    /// Status detail for the partial transcript.
    fn reason(&self, processed_ms: u64) -> String {
        match self {
            Self::Cancelled => cancelled_reason(processed_ms),
            Self::Failed { message, .. } => message.clone(),
        }
    }

    fn into_error(self, partial: Option<Box<PipelineOutput>>) -> PipelineError {
        match self {
            Self::Cancelled => PipelineError::Cancelled { partial },
            Self::Failed { code, message } => PipelineError::Failed {
                code,
                message,
                partial,
            },
        }
    }
}

fn cancelled_reason(processed_ms: u64) -> String {
    format!("Cancelled at {}", format_clock(processed_ms))
}

/// What a chunk's transcription gives up with.
#[derive(Debug)]
enum ChunkFailure {
    /// The job must stop (cancel, model switch, server problem).
    Stop(Stop),
    /// This chunk could not be transcribed; the job may go on.
    Failed(anyhow::Error),
}

/// Progress of the chunk loop.
#[derive(Default)]
struct ChunkRun {
    transcript: TranscriptBuilder,
    warnings: Vec<String>,
    chunks: u32,
    skipped_chunks: u32,
    failed_chunks: u32,
    /// End of the last finished (or skipped, or failed) chunk.
    processed_ms: u64,
}

impl ChunkRun {
    /// Why a run that decoded the whole file still failed: chunks failed and
    /// none produced text, so an empty transcript would hide an engine
    /// failure. None when any chunk has text, or no chunk failed (silence).
    fn engine_failure(&self) -> Option<String> {
        if self.failed_chunks == 0 || self.transcript.has_text() {
            return None;
        }
        let last = self.warnings.last().map(String::as_str).unwrap_or_default();
        Some(match self.failed_chunks {
            1 => last.to_string(),
            failed => format!("{} chunks could not be transcribed. {}", failed, last),
        })
    }

    /// The result so far, marked partial with `reason`; None when no chunk
    /// was finished.
    fn partial(
        &self,
        job: &JobInfo<'_>,
        outcome: Option<&DecodeOutcome>,
        reason: String,
    ) -> Option<Box<PipelineOutput>> {
        if self.chunks == 0 {
            return None;
        }
        let duration_ms = match outcome {
            Some(outcome) => outcome.samples / SAMPLES_PER_MS,
            None => job
                .estimated_duration_ms
                .unwrap_or(0)
                .max(self.processed_ms),
        };
        let mut details = self.warnings.clone();
        details.push(reason);
        let decoder = outcome.map_or(job.fallback_decoder, |outcome| outcome.decoder);
        Some(Box::new(self.output(
            job,
            duration_ms,
            decoder,
            Some(details),
        )))
    }

    fn finish(mut self, job: &JobInfo<'_>, outcome: &DecodeOutcome) -> PipelineOutput {
        let duration_ms = outcome.samples / SAMPLES_PER_MS;
        if let Some(reason) = &outcome.truncated {
            self.warnings.push(format!(
                "Decoding stopped early at {}: {}",
                format_clock(duration_ms),
                reason
            ));
        }
        let details = (!self.warnings.is_empty()).then(|| self.warnings.clone());
        self.output(job, duration_ms, outcome.decoder, details)
    }

    fn output(
        &self,
        job: &JobInfo<'_>,
        duration_ms: u64,
        decoder: DecoderKind,
        details: Option<Vec<String>>,
    ) -> PipelineOutput {
        let meta = TranscriptMeta {
            source_path: &job.source_path,
            source_name: &job.source_name,
            model_id: job.model_id,
            duration_ms,
            created_at: chrono::Utc::now().timestamp(),
        };
        PipelineOutput {
            transcript: self
                .transcript
                .build(&meta, details.map(|details| details.join("; "))),
            decoder,
            chunks: self.chunks,
            skipped_chunks: self.skipped_chunks,
            failed_chunks: self.failed_chunks,
            warnings: self.warnings.clone(),
            source_size_bytes: job.source_size_bytes,
            source_modified_at: job.source_modified_at,
            speakers: None,
        }
    }
}

/// Runs on the `file-decode` thread: decodes the whole file into the chunker
/// and sends each chunk to the engine thread.
fn decode_into_chunks(
    path: &Path,
    options: &DecodeOptions,
    token: &WorkToken,
    max_chunk_ms: u32,
    search_window_ms: u32,
    tx: SyncSender<AudioChunk>,
) -> (Result<DecodeOutcome, DecodeError>, anyhow::Result<()>) {
    let mut chunker = SilenceChunker::new(max_chunk_ms, search_window_ms);
    let receiver_gone = Cell::new(false);
    let mut send = |chunk: AudioChunk| {
        tx.send(chunk).map_err(|_| {
            receiver_gone.set(true);
            anyhow!("transcription stopped")
        })
    };
    let decoded = decode::decode_to_16k_mono(path, options, token.flag(), &mut |samples| {
        chunker.push(samples, &mut send)
    });
    // The buffered tail is still audio, unless nobody wants it any more.
    let finished = if matches!(decoded, Err(DecodeError::Cancelled)) || receiver_gone.get() {
        Ok(())
    } else {
        chunker.finish(&mut send)
    };
    (decoded, finished)
}

/// Runs on the calling thread: transcribes chunks until the decoder is done
/// (`None`) or the job has to stop.
fn transcribe_chunks(
    engine: &ChunkEngine<'_>,
    rx: &Receiver<AudioChunk>,
    gate: &mut SpeechGate,
    run: &mut ChunkRun,
    on_event: &mut dyn FnMut(PipelineEvent),
) -> Option<Stop> {
    let mut consecutive_failures = 0;
    for chunk in rx.iter() {
        if engine.token.is_cancelled() {
            return Some(Stop::Cancelled);
        }
        let index = chunk.index;
        let start_ms = chunk.start_ms();
        let len_ms = chunk.len_ms();
        let end_ms = start_ms + len_ms;
        let verdict = gate.prepare(&chunk.samples);
        drop(chunk);
        run.chunks += 1;

        let samples = match verdict {
            ChunkVerdict::Transcribe(samples) => samples,
            ChunkVerdict::SkipSilent => {
                run.skipped_chunks += 1;
                run.processed_ms = end_ms;
                on_event(PipelineEvent::Chunk {
                    index,
                    paragraph: None,
                    segments: Vec::new(),
                    language: None,
                    processed_ms: end_ms,
                    skipped: true,
                });
                continue;
            }
        };

        match engine.transcribe(&samples, index, start_ms, len_ms, on_event) {
            Ok(transcript) => {
                consecutive_failures = 0;
                let ChunkTranscript {
                    text,
                    segments,
                    grain,
                    language,
                } = transcript;
                let paragraph =
                    (!text.is_empty()).then(|| TranscriptSegment::new(start_ms, end_ms, text));
                run.transcript.push_chunk(
                    paragraph.clone(),
                    segments.clone(),
                    language.clone(),
                    grain,
                );
                run.processed_ms = end_ms;
                on_event(PipelineEvent::Chunk {
                    index,
                    paragraph,
                    segments,
                    language,
                    processed_ms: end_ms,
                    skipped: false,
                });
            }
            Err(ChunkFailure::Stop(stop)) => return Some(stop),
            Err(ChunkFailure::Failed(error)) => {
                consecutive_failures += 1;
                run.failed_chunks += 1;
                run.processed_ms = end_ms;
                run.warnings.push(format!(
                    "Could not transcribe {}–{}: {:#}",
                    format_clock(start_ms),
                    format_clock(end_ms),
                    error
                ));
                on_event(PipelineEvent::Chunk {
                    index,
                    paragraph: None,
                    segments: Vec::new(),
                    language: None,
                    processed_ms: end_ms,
                    skipped: false,
                });
                if consecutive_failures >= MAX_CONSECUTIVE_FAILED_CHUNKS {
                    return Some(Stop::Failed {
                        code: FileJobErrorCode::EngineFailed,
                        message: format!(
                            "{} chunks in a row could not be transcribed: {:#}",
                            consecutive_failures, error
                        ),
                    });
                }
            }
        }
    }
    None
}

impl ChunkEngine<'_> {
    /// Sends one chunk to the engine: waits while dictation is active (for
    /// an engine that yields to it), and retries a failed chunk once.
    fn transcribe(
        &self,
        samples: &[f32],
        index: u32,
        offset_ms: u64,
        real_len_ms: u64,
        on_event: &mut dyn FnMut(PipelineEvent),
    ) -> Result<ChunkTranscript, ChunkFailure> {
        let yields = self.engine.yields_to_dictation();
        let mut attempts = 0;
        let mut waiting = false;
        loop {
            if self.token.is_cancelled() {
                return Err(ChunkFailure::Stop(Stop::Cancelled));
            }
            if yields && (self.dictation_active)() {
                if !waiting {
                    waiting = true;
                    on_event(PipelineEvent::Waiting);
                }
                thread::sleep(DICTATION_POLL_INTERVAL);
                continue;
            }
            waiting = false;

            on_event(PipelineEvent::Transcribing {
                chunk_index: index,
                processed_ms: offset_ms,
            });
            match self.engine.transcribe(samples, offset_ms, real_len_ms) {
                Ok(transcript) => return Ok(transcript),
                // Dictation started between our check and the engine's: wait for it.
                Err(TranscribeError::Yield) => continue,
                Err(TranscribeError::Cancelled) => return Err(ChunkFailure::Stop(Stop::Cancelled)),
                Err(TranscribeError::Stop { code, message }) => {
                    return Err(ChunkFailure::Stop(Stop::Failed { code, message }))
                }
                Err(TranscribeError::Failed(error)) => {
                    attempts += 1;
                    if attempts >= ATTEMPTS_PER_CHUNK {
                        return Err(ChunkFailure::Failed(error));
                    }
                    debug!("Chunk {} failed, retrying: {:#}", index, error);
                }
            }
        }
    }
}

/// The chunk length and cut-search window to use, given the caller's request
/// and the model's limits.
fn chunk_plan(requested_max_ms: Option<u32>, limits: ChunkLimits) -> (u32, u32) {
    let max_chunk_ms = match requested_max_ms.unwrap_or(limits.max_chunk_ms) {
        0 => return (0, 0),
        // The chunker cannot cut shorter than this. Callers validate; this
        // only keeps a bad value from panicking the decoder thread.
        max => max.max(MIN_CHUNK_MS),
    };
    (max_chunk_ms, limits.search_window_ms.min(max_chunk_ms / 2))
}

fn decode_error_code(error: &DecodeError) -> Option<FileJobErrorCode> {
    match error {
        DecodeError::NotFound(_) => Some(FileJobErrorCode::NotFound),
        DecodeError::Unsupported(_) => Some(FileJobErrorCode::UnsupportedFormat),
        DecodeError::NeedsFfmpeg(_) => Some(FileJobErrorCode::NeedsFfmpeg),
        DecodeError::Failed(_) => Some(FileJobErrorCode::DecodeFailed),
        DecodeError::Cancelled => None,
    }
}

fn decode_failure(error: DecodeError, partial: Option<Box<PipelineOutput>>) -> PipelineError {
    match decode_error_code(&error) {
        Some(code) => PipelineError::Failed {
            code,
            message: error.to_string(),
            partial,
        },
        None => PipelineError::Cancelled { partial },
    }
}

/// `NoModel` when the model is unknown or not downloaded (the messages of
/// `TranscriptionManager::load_model_with_device`); `EngineFailed` otherwise.
fn model_load_error_code(error: &anyhow::Error) -> FileJobErrorCode {
    let message = format!("{:#}", error);
    if message.contains("Model not found") || message.contains("Model not downloaded") {
        FileJobErrorCode::NoModel
    } else {
        FileJobErrorCode::EngineFailed
    }
}

fn route_decoder(route: DecodeRoute) -> DecoderKind {
    match route {
        DecodeRoute::Symphonia => DecoderKind::Symphonia,
        DecodeRoute::Ffmpeg => DecoderKind::Ffmpeg,
    }
}

/// The source path as stored in transcripts and history: absolute when it can
/// be made so, so that it still points at the file later.
pub(crate) fn source_path_string(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The file name shown for a source (the whole path if it has none).
pub(crate) fn source_display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Everything about a transcript that does not come from its chunks.
pub(crate) struct TranscriptMeta<'a> {
    pub source_path: &'a str,
    pub source_name: &'a str,
    pub model_id: &'a str,
    pub duration_ms: u64,
    pub created_at: i64,
}

/// Collects the transcribed chunks of a job, in order, and assembles them
/// into a `FileTranscript`. Shared by the pipeline (final and partial
/// results) and the job manager (the live view of a running job).
#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptBuilder {
    paragraphs: Vec<TranscriptSegment>,
    segments: Vec<TranscriptSegment>,
    /// One per chunk with text whose language is known, in chunk order.
    languages: Vec<String>,
    /// The coarsest grain over chunks with text.
    grain: Option<TimingGrain>,
}

impl TranscriptBuilder {
    /// Adds one transcribed chunk. A chunk without text (`paragraph` None)
    /// adds nothing: its language guess and grain say nothing about speech.
    pub fn push_chunk(
        &mut self,
        paragraph: Option<TranscriptSegment>,
        segments: Vec<TranscriptSegment>,
        language: Option<String>,
        grain: TimingGrain,
    ) {
        let Some(paragraph) = paragraph else {
            return;
        };
        self.paragraphs.push(paragraph);
        self.segments.extend(segments);
        self.languages.extend(language);
        self.grain = Some(self.grain.map_or(grain, |current| current.min(grain)));
    }

    pub fn has_text(&self) -> bool {
        !self.paragraphs.is_empty()
    }

    /// A `status_detail` makes the transcript `Partial`.
    pub fn build(
        &self,
        meta: &TranscriptMeta<'_>,
        status_detail: Option<String>,
    ) -> FileTranscript {
        let text = self
            .paragraphs
            .iter()
            .map(|paragraph| paragraph.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        FileTranscript {
            source_path: meta.source_path.to_string(),
            source_name: meta.source_name.to_string(),
            duration_ms: meta.duration_ms,
            model_id: meta.model_id.to_string(),
            model_name: None,
            language: majority_language(&self.languages),
            status: if status_detail.is_some() {
                TranscriptStatus::Partial
            } else {
                TranscriptStatus::Completed
            },
            status_detail,
            timing: self.grain.unwrap_or(TimingGrain::Segment),
            text,
            paragraphs: self.paragraphs.clone(),
            segments: self.segments.clone(),
            created_at: meta.created_at,
            history_id: None,
            speakers: None,
        }
    }
}

/// The most frequent language; ties go to the one seen first.
fn majority_language(languages: &[String]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for language in languages {
        match counts.iter_mut().find(|(seen, _)| seen == language) {
            Some((_, count)) => *count += 1,
            None => counts.push((language, 1)),
        }
    }
    counts
        .into_iter()
        .fold(
            None,
            |best: Option<(&str, usize)>, (language, count)| match best {
                Some((_, best_count)) if best_count >= count => best,
                _ => Some((language, count)),
            },
        )
        .map(|(language, _)| language.to_string())
}

/// The timing grain of a finished chunk, for code that only sees its
/// `PipelineEvent::Chunk`: a chunk-level transcript is a single segment
/// spanning the whole paragraph.
pub(crate) fn chunk_grain(
    paragraph: &TranscriptSegment,
    segments: &[TranscriptSegment],
) -> TimingGrain {
    match segments {
        [only] if only == paragraph => TimingGrain::Chunk,
        _ => TimingGrain::Segment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::lemonade::tests::{
        begin_with_settings, json, MockServer, Reply, MODELS,
    };
    use std::cell::RefCell;

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment::new(start_ms, end_ms, text)
    }

    fn meta() -> TranscriptMeta<'static> {
        TranscriptMeta {
            source_path: "/audio/interview.m4a",
            source_name: "interview.m4a",
            model_id: "whisper-small",
            duration_ms: 60_000,
            created_at: 1_700_000_000,
        }
    }

    fn limits(max_chunk_ms: u32, search_window_ms: u32) -> ChunkLimits {
        ChunkLimits {
            max_chunk_ms,
            search_window_ms,
            hard_max_ms: None,
        }
    }

    fn languages(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|code| code.to_string()).collect()
    }

    #[test]
    fn majority_language_prefers_most_frequent_then_earliest() {
        assert_eq!(majority_language(&[]), None);
        assert_eq!(
            majority_language(&languages(&["de"])).as_deref(),
            Some("de")
        );
        assert_eq!(
            majority_language(&languages(&["en", "de", "de", "en", "de"])).as_deref(),
            Some("de")
        );
        assert_eq!(
            majority_language(&languages(&["fr", "en", "en", "fr"])).as_deref(),
            Some("fr")
        );
        assert_eq!(
            majority_language(&languages(&["ja", "en", "zh"])).as_deref(),
            Some("ja")
        );
    }

    #[test]
    fn builder_joins_paragraphs_and_keeps_segment_order() {
        let mut builder = TranscriptBuilder::default();
        builder.push_chunk(
            Some(segment(0, 28_000, "First part.")),
            vec![segment(100, 5_000, "First"), segment(5_000, 9_000, "part.")],
            Some("en".to_string()),
            TimingGrain::Segment,
        );
        builder.push_chunk(None, Vec::new(), Some("de".to_string()), TimingGrain::Chunk);
        builder.push_chunk(
            Some(segment(40_000, 60_000, "Second part.")),
            vec![segment(41_000, 59_000, "Second part.")],
            None,
            TimingGrain::Segment,
        );

        let transcript = builder.build(&meta(), None);

        assert_eq!(transcript.text, "First part.\n\nSecond part.");
        assert_eq!(
            transcript.paragraphs,
            vec![
                segment(0, 28_000, "First part."),
                segment(40_000, 60_000, "Second part.")
            ]
        );
        assert_eq!(
            transcript.segments,
            vec![
                segment(100, 5_000, "First"),
                segment(5_000, 9_000, "part."),
                segment(41_000, 59_000, "Second part.")
            ]
        );
        // The textless chunk's guesses do not count.
        assert_eq!(transcript.language.as_deref(), Some("en"));
        assert_eq!(transcript.timing, TimingGrain::Segment);
        assert_eq!(transcript.status, TranscriptStatus::Completed);
        assert_eq!(transcript.status_detail, None);
        assert_eq!(transcript.source_path, "/audio/interview.m4a");
        assert_eq!(transcript.source_name, "interview.m4a");
        assert_eq!(transcript.model_id, "whisper-small");
        assert_eq!(transcript.model_name, None);
        assert_eq!(transcript.duration_ms, 60_000);
        assert_eq!(transcript.created_at, 1_700_000_000);
        assert_eq!(transcript.history_id, None);
    }

    #[test]
    fn builder_grain_is_the_minimum_over_chunks_with_text() {
        let mut builder = TranscriptBuilder::default();
        assert_eq!(builder.build(&meta(), None).timing, TimingGrain::Segment);

        builder.push_chunk(
            Some(segment(0, 10_000, "a")),
            vec![segment(1_000, 9_000, "a")],
            None,
            TimingGrain::Segment,
        );
        assert_eq!(builder.build(&meta(), None).timing, TimingGrain::Segment);

        builder.push_chunk(
            Some(segment(10_000, 20_000, "b")),
            vec![segment(10_000, 20_000, "b")],
            None,
            TimingGrain::Chunk,
        );
        builder.push_chunk(
            Some(segment(20_000, 30_000, "c")),
            vec![segment(21_000, 29_000, "c")],
            None,
            TimingGrain::Segment,
        );
        assert_eq!(builder.build(&meta(), None).timing, TimingGrain::Chunk);
    }

    #[test]
    fn builder_status_detail_marks_partial() {
        let mut builder = TranscriptBuilder::default();
        assert!(!builder.has_text());
        builder.push_chunk(None, Vec::new(), None, TimingGrain::Chunk);
        assert!(!builder.has_text());
        builder.push_chunk(
            Some(segment(0, 1_000, "hi")),
            vec![segment(0, 1_000, "hi")],
            None,
            TimingGrain::Chunk,
        );
        assert!(builder.has_text());

        let transcript = builder.build(&meta(), Some("first; second".to_string()));

        assert_eq!(transcript.status, TranscriptStatus::Partial);
        assert_eq!(transcript.status_detail.as_deref(), Some("first; second"));
        assert_eq!(transcript.text, "hi");
    }

    #[test]
    fn empty_builder_gives_an_empty_completed_transcript() {
        let transcript = TranscriptBuilder::default().build(&meta(), None);

        assert_eq!(transcript.text, "");
        assert!(transcript.paragraphs.is_empty());
        assert!(transcript.segments.is_empty());
        assert_eq!(transcript.language, None);
        assert_eq!(transcript.status, TranscriptStatus::Completed);
    }

    #[test]
    fn chunk_run_output_reports_counts_and_partial_status() {
        let job = JobInfo {
            source_path: "/audio/a.mp3".to_string(),
            source_name: "a.mp3".to_string(),
            model_id: "tiny",
            estimated_duration_ms: Some(90_000),
            fallback_decoder: DecoderKind::Symphonia,
            source_size_bytes: 42,
            source_modified_at: Some(7),
        };
        let mut run = ChunkRun::default();
        assert!(run.partial(&job, None, "Cancelled".to_string()).is_none());

        run.chunks = 3;
        run.skipped_chunks = 1;
        run.failed_chunks = 1;
        run.processed_ms = 30_000;
        run.warnings.push("Could not transcribe x".to_string());
        run.transcript.push_chunk(
            Some(segment(0, 10_000, "hello")),
            vec![segment(0, 10_000, "hello")],
            Some("en".to_string()),
            TimingGrain::Chunk,
        );

        let partial = run
            .partial(&job, None, "Cancelled".to_string())
            .expect("partial result");
        assert_eq!(partial.chunks, 3);
        assert_eq!(partial.skipped_chunks, 1);
        assert_eq!(partial.failed_chunks, 1);
        assert_eq!(partial.decoder, DecoderKind::Symphonia);
        assert_eq!(partial.source_size_bytes, 42);
        assert_eq!(partial.source_modified_at, Some(7));
        assert_eq!(partial.warnings, vec!["Could not transcribe x".to_string()]);
        assert_eq!(partial.transcript.duration_ms, 90_000);
        assert_eq!(partial.transcript.status, TranscriptStatus::Partial);
        assert_eq!(
            partial.transcript.status_detail.as_deref(),
            Some("Could not transcribe x; Cancelled")
        );
        assert_eq!(partial.transcript.text, "hello");
        assert_eq!(partial.transcript.language.as_deref(), Some("en"));
        assert_eq!(partial.transcript.timing, TimingGrain::Chunk);

        let outcome = DecodeOutcome {
            decoder: DecoderKind::Wav,
            samples: 16 * 95_000,
            truncated: None,
        };
        run.warnings.clear();
        let done = run.finish(&job, &outcome);
        assert_eq!(done.decoder, DecoderKind::Wav);
        assert_eq!(done.transcript.duration_ms, 95_000);
        assert_eq!(done.transcript.status, TranscriptStatus::Completed);
        assert_eq!(done.transcript.status_detail, None);
    }

    #[test]
    fn engine_failure_only_when_failed_chunks_left_no_text() {
        let mut run = ChunkRun::default();
        // An empty or all-silent file is an empty transcript, not a failure.
        assert_eq!(run.engine_failure(), None);
        run.chunks = 2;
        run.skipped_chunks = 2;
        assert_eq!(run.engine_failure(), None);

        run.chunks = 3;
        run.failed_chunks = 1;
        run.warnings
            .push("Could not transcribe 00:00:00–00:00:25: worker crashed".to_string());
        assert_eq!(
            run.engine_failure().as_deref(),
            Some("Could not transcribe 00:00:00–00:00:25: worker crashed")
        );

        run.chunks = 4;
        run.failed_chunks = 2;
        run.warnings
            .push("Could not transcribe 00:00:25–00:00:50: GPU fault".to_string());
        assert_eq!(
            run.engine_failure().as_deref(),
            Some("2 chunks could not be transcribed. Could not transcribe 00:00:25–00:00:50: GPU fault")
        );

        // Once a chunk has text, the failures make the transcript partial.
        run.transcript.push_chunk(
            Some(segment(50_000, 60_000, "hello")),
            vec![segment(50_000, 60_000, "hello")],
            None,
            TimingGrain::Chunk,
        );
        assert_eq!(run.engine_failure(), None);
    }

    #[test]
    fn empty_decode_finishes_as_an_empty_completed_transcript() {
        let job = JobInfo {
            source_path: "/audio/empty.wav".to_string(),
            source_name: "empty.wav".to_string(),
            model_id: "tiny",
            estimated_duration_ms: Some(0),
            fallback_decoder: DecoderKind::Symphonia,
            source_size_bytes: 44,
            source_modified_at: None,
        };
        let run = ChunkRun::default();
        assert_eq!(run.engine_failure(), None);

        let done = run.finish(
            &job,
            &DecodeOutcome {
                decoder: DecoderKind::Wav,
                samples: 0,
                truncated: None,
            },
        );

        assert_eq!(done.chunks, 0);
        assert!(done.warnings.is_empty());
        assert_eq!(done.transcript.status, TranscriptStatus::Completed);
        assert_eq!(done.transcript.status_detail, None);
        assert_eq!(done.transcript.duration_ms, 0);
        assert_eq!(done.transcript.text, "");
    }

    #[test]
    fn chunk_grain_detects_single_paragraph_segment() {
        let paragraph = segment(0, 28_000, "Hello there.");

        assert_eq!(
            chunk_grain(&paragraph, std::slice::from_ref(&paragraph)),
            TimingGrain::Chunk
        );
        assert_eq!(
            chunk_grain(&paragraph, &[segment(300, 27_000, "Hello there.")]),
            TimingGrain::Segment
        );
        assert_eq!(
            chunk_grain(
                &paragraph,
                &[segment(0, 1_000, "Hello"), segment(1_000, 28_000, "there.")]
            ),
            TimingGrain::Segment
        );
    }

    #[test]
    fn exit_codes_follow_error_codes() {
        let usage = [
            FileJobErrorCode::NotFound,
            FileJobErrorCode::UnsupportedFormat,
            FileJobErrorCode::NeedsFfmpeg,
            FileJobErrorCode::DecodeFailed,
            FileJobErrorCode::NoModel,
            FileJobErrorCode::ServerUnauthorized,
            FileJobErrorCode::ServerModelMissing,
        ];
        for code in usage {
            assert_eq!(PipelineError::failed(code, "x").exit_code(), 2, "{code:?}");
        }
        let runtime = [
            FileJobErrorCode::ModelChanged,
            FileJobErrorCode::EngineFailed,
            FileJobErrorCode::ServerUnreachable,
            FileJobErrorCode::Internal,
        ];
        for code in runtime {
            assert_eq!(PipelineError::failed(code, "x").exit_code(), 1, "{code:?}");
        }
        assert_eq!(PipelineError::Cancelled { partial: None }.exit_code(), 1);
    }

    #[test]
    fn pipeline_error_displays_its_message() {
        assert_eq!(
            PipelineError::failed(FileJobErrorCode::NoModel, "No model").to_string(),
            "No model"
        );
        assert_eq!(
            PipelineError::Cancelled { partial: None }.to_string(),
            "Transcription was cancelled"
        );
    }

    #[test]
    fn chunk_plan_uses_model_limits_by_default() {
        assert_eq!(chunk_plan(None, limits(28_000, 10_000)), (28_000, 10_000));
        assert_eq!(chunk_plan(None, limits(24_000, 10_000)), (24_000, 10_000));
    }

    #[test]
    fn chunk_plan_scales_window_for_requested_lengths() {
        assert_eq!(
            chunk_plan(Some(60_000), limits(28_000, 10_000)),
            (60_000, 10_000)
        );
        assert_eq!(
            chunk_plan(Some(8_000), limits(28_000, 10_000)),
            (8_000, 4_000)
        );
        assert_eq!(chunk_plan(Some(0), limits(28_000, 10_000)), (0, 0));
        // Too short for the chunker: raised to its minimum.
        assert_eq!(
            chunk_plan(Some(3_000), limits(28_000, 10_000)),
            (5_000, 2_500)
        );
    }

    #[test]
    fn decode_errors_map_to_job_error_codes() {
        let cases = [
            (
                DecodeError::NotFound("x".into()),
                Some(FileJobErrorCode::NotFound),
            ),
            (
                DecodeError::Unsupported("x".into()),
                Some(FileJobErrorCode::UnsupportedFormat),
            ),
            (
                DecodeError::NeedsFfmpeg("x".into()),
                Some(FileJobErrorCode::NeedsFfmpeg),
            ),
            (
                DecodeError::Failed(anyhow!("x")),
                Some(FileJobErrorCode::DecodeFailed),
            ),
            (DecodeError::Cancelled, None),
        ];
        for (error, expected) in cases {
            assert_eq!(decode_error_code(&error), expected, "{error:?}");
        }
    }

    #[test]
    fn model_load_errors_distinguish_missing_models() {
        assert_eq!(
            model_load_error_code(&anyhow!("Model not found: tiny")),
            FileJobErrorCode::NoModel
        );
        assert_eq!(
            model_load_error_code(&anyhow!("Model not downloaded").context("loading tiny")),
            FileJobErrorCode::NoModel
        );
        assert_eq!(
            model_load_error_code(&anyhow!("GPU out of memory")),
            FileJobErrorCode::EngineFailed
        );
    }

    #[test]
    fn source_names() {
        assert_eq!(
            source_display_name(Path::new("/audio/Interview 1.M4A")),
            "Interview 1.M4A"
        );
        assert!(Path::new(&source_path_string(Path::new("rel/a.wav"))).is_absolute());
        #[cfg(unix)]
        assert_eq!(
            source_path_string(Path::new("/audio/a.wav")),
            "/audio/a.wav"
        );
    }

    /// A 16 kHz mono WAV of 1 kHz tone bursts: `on_s` seconds on, 1 s off.
    fn tone_bursts(path: &Path, total_s: u32, on_s: u32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for n in 0..total_s * 16_000 {
            let in_burst = (n / 16_000) % (on_s + 1) < on_s;
            let value = if in_burst {
                (n as f32 * 2.0 * std::f32::consts::PI * 1_000.0 / 16_000.0).sin() * 0.3
            } else {
                0.0
            };
            writer.write_sample((value * 32767.0) as i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn lemonade_options(server: &MockServer, settings: AppSettings) -> PipelineOptions {
        PipelineOptions {
            engine: PipelineEngine::Lemonade {
                config: LemonadeJobConfig {
                    base_url: server.url(),
                    model: "Whisper-Base".to_string(),
                    api_key: None,
                },
                settings,
            },
            max_chunk_ms: None,
            ffmpeg: None,
            // Tones are not speech to the VAD.
            skip_silent_chunks: false,
            speaker_model: None,
        }
    }

    fn chunk_reply(n: u32) -> Reply {
        json(
            200,
            &format!(
                r#"{{"text":" Kubernetis part {n}.","language":"english",
                    "segments":[{{"start":0.5,"end":2.0,"text":" Kubernetis part {n}."}}]}}"#
            ),
        )
    }

    #[test]
    fn lemonade_job_transcribes_every_chunk_at_its_place_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bursts.wav");
        tone_bursts(&path, 65, 3);
        let server = MockServer::start(vec![
            json(200, MODELS),
            chunk_reply(1),
            chunk_reply(2),
            chunk_reply(3),
        ]);
        let settings = AppSettings {
            custom_words: vec!["Kubernetes".to_string()],
            ..Default::default()
        };
        let opts = lemonade_options(&server, settings.clone());
        let token = WorkToken::new();
        let engine = begin_with_settings(&server, "Whisper-Base", settings, token.clone()).unwrap();
        let decode_options = DecodeOptions::default();
        let probe = decode::probe(&path, &decode_options).unwrap();
        let events = RefCell::new(Vec::new());
        let dictation_polls = Cell::new(0);
        let dictation_active = || {
            dictation_polls.set(dictation_polls.get() + 1);
            true
        };

        let output = run_with(
            &engine,
            &dictation_active,
            &path,
            &decode_options,
            &probe,
            &opts,
            &token,
            &mut |event| events.borrow_mut().push(event),
        )
        .unwrap();

        // Lemonade never waits for dictation, even while one is active.
        assert_eq!(dictation_polls.get(), 0);
        assert!(!events
            .borrow()
            .iter()
            .any(|event| matches!(event, PipelineEvent::Waiting)));

        let transcript = &output.transcript;
        assert_eq!(output.chunks, 3);
        assert_eq!(output.failed_chunks, 0);
        assert_eq!(transcript.model_id, "lemonade:Whisper-Base");
        assert_eq!(transcript.duration_ms, 65_000);
        assert_eq!(transcript.status, TranscriptStatus::Completed);
        assert_eq!(transcript.language.as_deref(), Some("en"));
        assert_eq!(transcript.timing, TimingGrain::Segment);
        assert_eq!(
            transcript.text,
            "Kubernetes part 1.\n\nKubernetes part 2.\n\nKubernetes part 3."
        );
        // Chunks tile the file; each segment sits 0.5–2 s into its chunk.
        let mut chunk_start = 0;
        for (i, (paragraph, segment)) in transcript
            .paragraphs
            .iter()
            .zip(&transcript.segments)
            .enumerate()
        {
            assert_eq!(paragraph.start_ms, chunk_start, "chunk {i}");
            assert!(paragraph.end_ms - paragraph.start_ms <= 28_000, "chunk {i}");
            assert_eq!(segment.start_ms, chunk_start + 500, "chunk {i}");
            assert_eq!(segment.end_ms, chunk_start + 2_000, "chunk {i}");
            assert_eq!(segment.text, format!("Kubernetes part {}.", i + 1));
            chunk_start = paragraph.end_ms;
        }
        assert_eq!(chunk_start, 65_000);

        // One preflight, then one request per chunk, in order, each with
        // that chunk's audio.
        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method, "GET");
        for (request, paragraph) in requests[1..].iter().zip(&transcript.paragraphs) {
            assert_eq!(request.path, "/api/v1/audio/transcriptions");
            let samples = (paragraph.end_ms - paragraph.start_ms) as usize * 16;
            let wav_bytes = 44 + samples.max(16_000) * 2;
            let file_start = request.body.windows(4).position(|w| w == b"RIFF").unwrap();
            let data_len = u32::from_le_bytes(
                request.body[file_start + 40..file_start + 44]
                    .try_into()
                    .unwrap(),
            ) as usize;
            assert_eq!(44 + data_len, wav_bytes);
        }
    }

    #[test]
    fn a_failed_lemonade_chunk_is_retried_once() {
        let server = MockServer::start(vec![
            json(200, MODELS),
            json(500, r#"{"error":{"message":"simulated failure"}}"#),
            chunk_reply(1),
        ]);
        let token = WorkToken::new();
        let transcriber = begin_with_settings(
            &server,
            "Whisper-Base",
            AppSettings::default(),
            token.clone(),
        )
        .unwrap();
        let engine = ChunkEngine {
            engine: &transcriber,
            dictation_active: &|| false,
            token: &token,
        };
        let mut transcribing = 0;

        let chunk = engine
            .transcribe(&[0.1; 16_000], 0, 0, 1_000, &mut |event| {
                if matches!(event, PipelineEvent::Transcribing { .. }) {
                    transcribing += 1;
                }
            })
            .unwrap();

        assert_eq!(chunk.text, "Kubernetis part 1.");
        assert_eq!(transcribing, 2);
        assert_eq!(server.requests().len(), 3);
    }

    #[test]
    fn a_lemonade_server_problem_stops_the_job() {
        let server = MockServer::start(vec![
            json(200, MODELS),
            json(
                404,
                r#"{"error":{"message":"model Whisper-Base not found"}}"#,
            ),
        ]);
        let token = WorkToken::new();
        let transcriber = begin_with_settings(
            &server,
            "Whisper-Base",
            AppSettings::default(),
            token.clone(),
        )
        .unwrap();
        let engine = ChunkEngine {
            engine: &transcriber,
            dictation_active: &|| false,
            token: &token,
        };
        match engine.transcribe(&[0.1; 16_000], 0, 0, 1_000, &mut |_| {}) {
            Err(ChunkFailure::Stop(Stop::Failed { code, .. })) => {
                assert_eq!(code, FileJobErrorCode::ServerModelMissing)
            }
            Err(ChunkFailure::Stop(Stop::Cancelled)) => panic!("not cancelled"),
            Err(ChunkFailure::Failed(e)) => panic!("expected a stop, got {e:#}"),
            Ok(_) => panic!("expected a stop"),
        }
        // Only the one request: a stop is not retried.
        assert_eq!(server.requests().len(), 2);
    }

    /// A scripted engine for the chunk loop.
    struct FakeEngine {
        yields: bool,
        results: RefCell<Vec<Result<ChunkTranscript, TranscribeError>>>,
    }

    impl ChunkTranscriber for FakeEngine {
        fn model_id(&self) -> &str {
            "fake"
        }
        fn limits(&self) -> ChunkLimits {
            limits(28_000, 10_000)
        }
        fn yields_to_dictation(&self) -> bool {
            self.yields
        }
        fn transcribe(
            &self,
            _samples: &[f32],
            offset_ms: u64,
            real_len_ms: u64,
        ) -> Result<ChunkTranscript, TranscribeError> {
            self.results.borrow_mut().pop().unwrap_or_else(|| {
                Ok(ChunkTranscript {
                    text: "ok".to_string(),
                    segments: vec![segment(offset_ms, offset_ms + real_len_ms, "ok")],
                    grain: TimingGrain::Chunk,
                    language: None,
                })
            })
        }
    }

    #[test]
    fn an_engine_that_yields_waits_for_dictation() {
        let engine = FakeEngine {
            yields: true,
            // Popped from the end: a Yield first, then success.
            results: RefCell::new(vec![Err(TranscribeError::Yield)]),
        };
        let polls = Cell::new(0);
        // Active for the first two polls.
        let dictation_active = || {
            polls.set(polls.get() + 1);
            polls.get() <= 2
        };
        let token = WorkToken::new();
        let chunks = ChunkEngine {
            engine: &engine,
            dictation_active: &dictation_active,
            token: &token,
        };
        let mut events = Vec::new();

        let chunk = chunks
            .transcribe(&[0.0; 16_000], 0, 0, 1_000, &mut |event| events.push(event))
            .unwrap();

        assert_eq!(chunk.text, "ok");
        assert_eq!(polls.get(), 4);
        let waiting = events
            .iter()
            .filter(|event| matches!(event, PipelineEvent::Waiting))
            .count();
        assert_eq!(waiting, 1);
    }

    #[test]
    fn engine_stops_map_to_job_failures() {
        let token = WorkToken::new();
        let engine = FakeEngine {
            yields: false,
            results: RefCell::new(vec![Err(TranscribeError::Stop {
                code: FileJobErrorCode::ServerUnreachable,
                message: "down".to_string(),
            })]),
        };
        let chunks = ChunkEngine {
            engine: &engine,
            dictation_active: &|| true,
            token: &token,
        };
        assert!(matches!(
            chunks.transcribe(&[0.0; 16_000], 0, 0, 1_000, &mut |_| {}),
            Err(ChunkFailure::Stop(Stop::Failed {
                code: FileJobErrorCode::ServerUnreachable,
                ..
            }))
        ));

        assert!(matches!(
            engine_start_error(TranscribeError::Stop {
                code: FileJobErrorCode::ServerUnauthorized,
                message: "bad key".to_string(),
            }),
            PipelineError::Failed {
                code: FileJobErrorCode::ServerUnauthorized,
                ..
            }
        ));
        assert!(matches!(
            engine_start_error(TranscribeError::Cancelled),
            PipelineError::Cancelled { partial: None }
        ));
        assert!(matches!(
            engine_start_error(TranscribeError::Failed(anyhow!("boom"))),
            PipelineError::Failed {
                code: FileJobErrorCode::EngineFailed,
                ..
            }
        ));
    }

    #[test]
    fn engine_config_needs_a_model() {
        let handy = |model_id: &str| PipelineEngine::Handy {
            model_id: model_id.to_string(),
            device_index: None,
        };
        let lemonade = |model: &str| PipelineEngine::Lemonade {
            config: LemonadeJobConfig {
                base_url: "http://localhost:13305".to_string(),
                model: model.to_string(),
                api_key: None,
            },
            settings: AppSettings::default(),
        };
        assert!(check_engine_config(&handy("small")).is_ok());
        assert!(check_engine_config(&lemonade("Whisper-Base")).is_ok());
        for (engine, message) in [
            (handy(""), "No transcription model is selected"),
            (lemonade(""), "No Lemonade model is selected"),
        ] {
            match check_engine_config(&engine) {
                Err(PipelineError::Failed {
                    code, message: m, ..
                }) => {
                    assert_eq!(code, FileJobErrorCode::NoModel);
                    assert_eq!(m, message);
                }
                other => panic!("expected NoModel, got {other:?}"),
            }
        }
    }

    fn output_with(paragraphs: Vec<TranscriptSegment>) -> PipelineOutput {
        let mut builder = TranscriptBuilder::default();
        for paragraph in paragraphs {
            builder.push_chunk(
                Some(paragraph.clone()),
                vec![paragraph],
                Some("en".to_string()),
                TimingGrain::Chunk,
            );
        }
        PipelineOutput {
            transcript: builder.build(&meta(), None),
            decoder: DecoderKind::Wav,
            chunks: 1,
            skipped_chunks: 0,
            failed_chunks: 0,
            warnings: Vec::new(),
            source_size_bytes: 1,
            source_modified_at: None,
            speakers: None,
        }
    }

    /// Starts labelling a file that can't be diarized (no such model or audio).
    fn failing_labels(token: &WorkToken, events: &mut Vec<PipelineEvent>) -> SpeakerLabels {
        let dir = std::env::temp_dir().join("handy-pipeline-no-such-dir");
        SpeakerLabels::start(
            Some(&dir.join("no-such-model.gguf")),
            &dir.join("no-such-audio.wav"),
            &DecodeOptions::default(),
            Some(60_000),
            token,
            Arc::new(|| false),
            &mut |event| events.push(event),
        )
    }

    #[test]
    fn a_speaker_labelling_failure_keeps_the_transcript() {
        let token = WorkToken::new();
        let mut events = Vec::new();
        let mut labels = failing_labels(&token, &mut events);
        assert!(matches!(
            events.as_slice(),
            [PipelineEvent::Speakers(SpeakerJobState::Running)]
        ));
        let output = output_with(vec![segment(0, 28_000, "Hello.")]);
        let before = output.transcript.clone();

        let output = labels
            .finish(Ok(output), &mut |event| events.push(event))
            .unwrap();

        let state = output.speakers.clone().expect("final speaker state");
        assert!(
            matches!(
                state,
                SpeakerJobState::Failed { .. } | SpeakerJobState::Skipped { .. }
            ),
            "{state:?}"
        );
        assert!(matches!(
            events.last(),
            Some(PipelineEvent::Speakers(last)) if *last == state
        ));
        // Same text, no labels, still completed.
        assert_eq!(output.transcript, before);
        assert_eq!(output.transcript.status, TranscriptStatus::Completed);
        assert_eq!(output.warnings.len(), 1);
        assert!(output.warnings[0].starts_with("Speaker labels: "));
    }

    #[test]
    fn an_empty_transcript_gets_no_labels() {
        let token = WorkToken::new();
        let mut events = Vec::new();
        let mut labels = failing_labels(&token, &mut events);

        let output = labels
            .finish(Ok(output_with(Vec::new())), &mut |event| events.push(event))
            .unwrap();

        let skipped = SpeakerJobState::Skipped {
            reason: SpeakerSkipReason::NoSpeech,
        };
        assert_eq!(output.speakers, Some(skipped.clone()));
        assert!(output.warnings.is_empty());
        assert!(matches!(events.last(), Some(PipelineEvent::Speakers(s)) if *s == skipped));
    }

    #[test]
    fn a_cancelled_job_stops_labelling_and_its_partial_is_unlabelled() {
        let token = WorkToken::new();
        let mut events = Vec::new();
        let mut labels = failing_labels(&token, &mut events);
        let partial = output_with(vec![segment(0, 28_000, "Hello.")]);

        let error = labels
            .finish(
                Err(PipelineError::Cancelled {
                    partial: Some(Box::new(partial)),
                }),
                &mut |event| events.push(event),
            )
            .unwrap_err();

        // Labelling is stopped through its own token; the job's token is the
        // job's to cancel, never the labeller's.
        assert!(!token.is_cancelled());
        let cancelled = SpeakerJobState::Failed {
            message: "cancelled".to_string(),
        };
        let PipelineError::Cancelled {
            partial: Some(partial),
        } = error
        else {
            panic!("expected a cancelled job with its partial");
        };
        assert_eq!(partial.speakers, Some(cancelled.clone()));
        assert!(partial.transcript.speakers.is_none());
        assert!(matches!(events.last(), Some(PipelineEvent::Speakers(s)) if *s == cancelled));
    }

    #[test]
    fn cancelling_while_waiting_for_labels_cancels_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let (model, audio) = (dir.path().join("model.gguf"), dir.path().join("a.wav"));
        std::fs::write(&model, b"not a model").unwrap();
        tone_bursts(&audio, 2, 1);
        let token = WorkToken::new();
        let mut events = Vec::new();
        // Held as if dictation never ends, so labels are still pending when
        // the transcript is done and the user cancels.
        let mut labels = SpeakerLabels::start(
            Some(&model),
            &audio,
            &DecodeOptions::default(),
            Some(2_000),
            &token,
            Arc::new(|| true),
            &mut |event| events.push(event),
        );
        let output = output_with(vec![segment(0, 2_000, "Hello.")]);
        let before = output.transcript.clone();
        let canceller = {
            let token = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                token.cancel();
            })
        };

        let error = labels
            .finish(Ok(output), &mut |event| events.push(event))
            .unwrap_err();
        canceller.join().unwrap();

        // Cancelled, not completed (so never saved to History), with the
        // whole transcript kept for viewing.
        let PipelineError::Cancelled {
            partial: Some(partial),
        } = error
        else {
            panic!("expected a cancelled job with its transcript, got {error:?}");
        };
        let cancelled = SpeakerJobState::Failed {
            message: "cancelled".to_string(),
        };
        assert_eq!(partial.speakers, Some(cancelled.clone()));
        assert_eq!(partial.transcript, before);
        assert!(partial.warnings.is_empty(), "{:?}", partial.warnings);
        assert!(matches!(events.last(), Some(PipelineEvent::Speakers(s)) if *s == cancelled));
    }

    #[test]
    fn no_labels_asked_means_no_speaker_events() {
        let mut events = Vec::new();
        let mut labels = SpeakerLabels::start(
            None,
            Path::new("/audio/a.wav"),
            &DecodeOptions::default(),
            None,
            &WorkToken::new(),
            Arc::new(|| false),
            &mut |event| events.push(event),
        );
        let output = labels
            .finish(
                Ok(output_with(vec![segment(0, 1_000, "Hi.")])),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert!(events.is_empty());
        assert_eq!(output.speakers, None);
    }
}
