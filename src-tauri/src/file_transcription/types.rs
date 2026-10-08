//! Types shared by every part of file transcription (decode, engine,
//! pipeline, history, commands, CLI, frontend). Any serde/specta change here
//! changes src/bindings.ts.

use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::Path;

/// Languages written without spaces between words: chunk and segment texts
/// are joined with "" instead of " ", and subtitle lines wrap by characters.
pub const NO_SPACE_LANGUAGES: &[&str] = &["zh", "ja", "yue", "th", "lo", "km", "my"];

/// `language` is an ISO code as produced by the engine ("zh", "zh-TW", "ja").
pub fn is_no_space_language(language: Option<&str>) -> bool {
    language.is_some_and(|lang| {
        let base = lang
            .split(['-', '_'])
            .next()
            .unwrap_or(lang)
            .to_ascii_lowercase();
        NO_SPACE_LANGUAGES.contains(&base.as_str())
    })
}

/// One timed piece of transcript text. Milliseconds from the start of the
/// source file; `end_ms > start_ms`; `text` is trimmed and non-empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// How precise segment times are. A job's grain is the minimum over its chunks.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Type,
)]
#[serde(rename_all = "snake_case")]
pub enum TimingGrain {
    /// One segment per chunk (about every 15–28 s): the model gave no timestamps.
    Chunk,
    /// Model-provided segment (phrase/sentence) timestamps.
    Segment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStatus {
    Completed,
    /// Something is missing (decoding stopped early, chunks failed, the job
    /// was cancelled or is still running). See `status_detail`.
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum DecoderKind {
    /// 16 kHz mono 16-bit PCM WAV read directly (same samples as read_wav_samples).
    Wav,
    Symphonia,
    Ffmpeg,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct FileTranscript {
    pub source_path: String,
    pub source_name: String,
    /// Decoded audio length (exact when finished; probe estimate while running).
    pub duration_ms: u64,
    pub model_id: String,
    pub model_name: Option<String>,
    /// ISO code of the transcript language, when known.
    pub language: Option<String>,
    pub status: TranscriptStatus,
    /// English explanation when `status` is `Partial`.
    pub status_detail: Option<String>,
    pub timing: TimingGrain,
    /// `paragraphs` texts joined by "\n\n": what Copy, TXT export and History use.
    pub text: String,
    /// One per transcribed (non-silent, non-empty) chunk: chunk start/end and
    /// its post-processed text (processed exactly like a dictation).
    pub paragraphs: Vec<TranscriptSegment>,
    /// Finer timed segments for subtitles and the timestamped view. For chunks
    /// without model timestamps the chunk's paragraph is its only segment.
    pub segments: Vec<TranscriptSegment>,
    /// Unix seconds when transcription finished (or the snapshot was taken).
    pub created_at: i64,
    pub history_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptFormat {
    Txt,
    #[value(alias = "markdown")]
    Md,
    Srt,
    Vtt,
    Json,
}

impl TranscriptFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Md => "md",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::Json => "json",
        }
    }

    /// Case-insensitive; accepts txt|text|md|markdown|srt|vtt|webvtt|json.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "txt" | "text" => Some(Self::Txt),
            "md" | "markdown" => Some(Self::Md),
            "srt" => Some(Self::Srt),
            "vtt" | "webvtt" => Some(Self::Vtt),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Where a transcript to view or export comes from.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptSource {
    Job { job_id: u64 },
    History { history_id: i64 },
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct SupportedAudioFormats {
    /// Decoded in-process. Lowercase extensions without dots.
    pub native: Vec<String>,
    /// Need ffmpeg. Lowercase extensions without dots.
    pub ffmpeg: Vec<String>,
    pub ffmpeg_available: bool,
    pub ffmpeg_path: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, Type)]
pub struct FileTranscriptionOptions {
    pub save_to_history: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FileJobStatus {
    Queued,
    /// Probing the file and loading the model.
    Preparing,
    /// Paused because a dictation is recording or being transcribed.
    Waiting,
    Transcribing,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FileJobErrorCode {
    NotFound,
    UnsupportedFormat,
    NeedsFfmpeg,
    NoModel,
    ModelChanged,
    DecodeFailed,
    EngineFailed,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct FileJobError {
    pub code: FileJobErrorCode,
    /// English detail for logs/tooltips; the UI shows a translated text per `code`.
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct FileJobSnapshot {
    pub job_id: u64,
    pub path: String,
    pub file_name: String,
    pub status: FileJobStatus,
    pub duration_ms: Option<u64>,
    /// End of the last finished (or skipped) chunk.
    pub processed_ms: u64,
    pub chunks_done: u32,
    pub decoder: Option<DecoderKind>,
    pub model_id: Option<String>,
    pub error: Option<FileJobError>,
    /// get_file_transcript(Job) returns something (final, partial or running).
    pub has_transcript: bool,
    pub transcript_status: Option<TranscriptStatus>,
    pub history_id: Option<i64>,
    /// Unix milliseconds.
    pub enqueued_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileTranscriptionEvent {
    /// Full job state after any change. Progress-only changes are throttled
    /// to at most one per 250 ms per job; status changes are never throttled.
    Updated { job: FileJobSnapshot },
    /// Post-processed segments of one finished chunk, in order. Append to the live view.
    Chunk {
        job_id: u64,
        chunk_index: u32,
        segments: Vec<TranscriptSegment>,
    },
    /// The job was dismissed (or evicted) and its result freed.
    Removed { job_id: u64 },
}

/// Present on HistoryEntry for entries made by file transcription.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct FileHistoryInfo {
    pub source_path: String,
    pub source_name: String,
    pub duration_ms: u64,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub status: TranscriptStatus,
    /// Whether source_path exists right now (checked when the entry is read).
    pub source_exists: bool,
}
