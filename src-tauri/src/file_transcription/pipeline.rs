//! The decode → chunk → transcribe pipeline shared by the GUI job manager and
//! the headless CLI.
//!
//! A `file-decode` thread decodes the file to 16 kHz mono and splits it into
//! silence-aware chunks, which reach the calling thread through a small
//! bounded channel, so memory stays flat whatever the file length. The
//! calling thread sends one chunk at a time to the engine and waits while a
//! dictation is active.

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::chunker::{
    AudioChunk, ChunkVerdict, SilenceChunker, SpeechGate, SAMPLES_PER_MS,
};
use crate::file_transcription::decode::{self, DecodeError, DecodeOptions, DecodeOutcome};
use crate::file_transcription::export::format_clock;
use crate::file_transcription::formats::DecodeRoute;
use crate::file_transcription::types::*;
use crate::managers::transcription::{
    ChunkError, ChunkLimits, ChunkTranscript, FileJobLease, TranscriptionManager,
};
use anyhow::anyhow;
use log::debug;
use std::cell::Cell;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
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

#[derive(Clone, Debug)]
pub struct PipelineOptions {
    pub model_id: String,
    pub device_index: Option<usize>,
    /// None = lease.limits().max_chunk_ms. Some(0) = whole file in one engine
    /// call. Some(n): n >= 5000, used as-is (callers warn if > hard_max_ms).
    pub max_chunk_ms: Option<u32>,
    pub ffmpeg: Option<PathBuf>,
    pub skip_silent_chunks: bool,
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

    /// 2 for NotFound | UnsupportedFormat | NeedsFfmpeg | DecodeFailed | NoModel; 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Failed {
                code:
                    FileJobErrorCode::NotFound
                    | FileJobErrorCode::UnsupportedFormat
                    | FileJobErrorCode::NeedsFfmpeg
                    | FileJobErrorCode::DecodeFailed
                    | FileJobErrorCode::NoModel,
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

    if opts.model_id.is_empty() {
        return Err(PipelineError::failed(
            FileJobErrorCode::NoModel,
            "No transcription model is selected",
        ));
    }
    if token.is_cancelled() {
        return Err(PipelineError::Cancelled { partial: None });
    }
    let lease = tm
        .begin_file_job(&opts.model_id, opts.device_index, token.clone())
        .map_err(|e| {
            if token.is_cancelled() {
                PipelineError::Cancelled { partial: None }
            } else {
                PipelineError::failed(
                    model_load_error_code(&e),
                    format!("Could not load model {}: {:#}", opts.model_id, e),
                )
            }
        })?;
    on_event(PipelineEvent::ModelReady);

    let (max_chunk_ms, search_window_ms) = chunk_plan(opts.max_chunk_ms, lease.limits());
    let mut gate = SpeechGate::new(opts.skip_silent_chunks).map_err(|e| {
        PipelineError::failed(
            FileJobErrorCode::Internal,
            format!("Could not start the speech detector: {:#}", e),
        )
    })?;
    debug!(
        "Transcribing {} with {} in chunks of at most {} ms",
        path.display(),
        opts.model_id,
        max_chunk_ms
    );

    let job = JobInfo {
        source_path: source_path_string(path),
        source_name: source_display_name(path),
        model_id: &opts.model_id,
        estimated_duration_ms: probe.duration_ms,
        fallback_decoder: route_decoder(probe.route),
        source_size_bytes: probe.source_size_bytes,
        source_modified_at: probe.source_modified_at,
    };
    let engine = ChunkEngine {
        tm,
        lease: &lease,
        token,
    };
    let mut run = ChunkRun::default();
    let (tx, rx) = mpsc::sync_channel::<AudioChunk>(CHUNK_QUEUE_LEN);

    thread::scope(|scope| {
        let decode_options = &decode_options;
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
        Ok(run.finish(&job, &outcome))
    })
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
    tm: &'a TranscriptionManager,
    lease: &'a FileJobLease,
    token: &'a WorkToken,
}

/// Why the chunk loop stopped before the decoder ran dry.
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
enum ChunkFailure {
    /// The job must stop (cancel, model switch).
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
                let paragraph = (!text.is_empty()).then_some(TranscriptSegment {
                    start_ms,
                    end_ms,
                    text,
                });
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
    /// Sends one chunk to the engine: waits while dictation is active, and
    /// retries a failed chunk once.
    fn transcribe(
        &self,
        samples: &[f32],
        index: u32,
        offset_ms: u64,
        real_len_ms: u64,
        on_event: &mut dyn FnMut(PipelineEvent),
    ) -> Result<ChunkTranscript, ChunkFailure> {
        let mut attempts = 0;
        let mut waiting = false;
        loop {
            if self.token.is_cancelled() {
                return Err(ChunkFailure::Stop(Stop::Cancelled));
            }
            if self.tm.dictation_active() {
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
            match self.lease.transcribe_chunk(samples, offset_ms, real_len_ms) {
                Ok(transcript) => return Ok(transcript),
                // Dictation started between our check and the engine's: wait for it.
                Err(ChunkError::Yield) => continue,
                Err(ChunkError::Cancelled) => return Err(ChunkFailure::Stop(Stop::Cancelled)),
                Err(ChunkError::ModelChanged {
                    job_model,
                    current_model,
                }) => {
                    return Err(ChunkFailure::Stop(Stop::Failed {
                        code: FileJobErrorCode::ModelChanged,
                        message: format!(
                            "The model changed from {} to {} during transcription",
                            job_model, current_model
                        ),
                    }))
                }
                Err(ChunkError::Failed(error)) => {
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

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
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
        ];
        for code in usage {
            assert_eq!(PipelineError::failed(code, "x").exit_code(), 2, "{code:?}");
        }
        let runtime = [
            FileJobErrorCode::ModelChanged,
            FileJobErrorCode::EngineFailed,
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
}
