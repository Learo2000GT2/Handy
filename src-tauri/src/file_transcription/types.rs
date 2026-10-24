//! Types shared by every part of file transcription (decode, engine,
//! pipeline, history, commands, CLI, frontend). Any serde/specta change here
//! changes src/bindings.ts.

use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::Path;

use crate::settings::FileTranscriptionEngine;

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
    /// Display speaker id (1-based, an id in `FileTranscript.speakers`) when
    /// speaker labels were made; absent from JSON otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<u32>,
}

impl TranscriptSegment {
    pub fn new(start_ms: u64, end_ms: u64, text: impl Into<String>) -> Self {
        Self {
            start_ms,
            end_ms,
            text: text.into(),
            speaker: None,
        }
    }
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
    /// Speaker labels ("who said what"); None when not requested or not made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speakers: Option<TranscriptSpeakers>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TranscriptSpeaker {
    /// Unique and 1-based. Numbered by first appearance when labels are made.
    /// Never renumbered by a merge or a passage change, so there can be gaps,
    /// and an id freed by a merge can be given to a new speaker.
    pub id: u32,
    /// User-given name; None shows "Speaker {id}".
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct TranscriptSpeakers {
    pub speakers: Vec<TranscriptSpeaker>,
    /// Labels were placed on sentences with interpolated times (the model
    /// gave no timestamps): expect mistakes near speaker changes.
    pub approximate: bool,
    /// Segments that overlapped no speaker turn and were left unlabeled.
    pub unassigned_segments: u32,
    /// Diarizer id, e.g. "sortformer-4spk-v2.1-q8_0".
    pub model: String,
}

/// Progress of speaker labelling for one file job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SpeakerJobState {
    Pending,
    Running,
    Done {
        speakers: u32,
    },
    /// The transcript was kept without labels.
    Failed {
        message: String,
    },
    Skipped {
        reason: SpeakerSkipReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerSkipReason {
    /// The speaker model isn't downloaded.
    ModelMissing,
    /// The diarizer found no speech, or no transcript text matched a speaker.
    NoSpeech,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct SpeakerModelStatus {
    pub downloaded: bool,
    pub downloading: bool,
    pub size_bytes: u64,
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpeakerModelEvent {
    Progress { downloaded: u64, total: u64 },
    Verifying,
    Completed,
    Failed { message: String },
    Cancelled,
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
    // A Word document: written to a file, never printed. (No doc comment,
    // so the binding stays a one-line union.)
    Docx,
}

impl TranscriptFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Md => "md",
            Self::Srt => "srt",
            Self::Vtt => "vtt",
            Self::Json => "json",
            Self::Docx => "docx",
        }
    }

    /// Case-insensitive; accepts txt|text|md|markdown|srt|vtt|webvtt|json|docx.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "txt" | "text" => Some(Self::Txt),
            "md" | "markdown" => Some(Self::Md),
            "srt" => Some(Self::Srt),
            "vtt" | "webvtt" => Some(Self::Vtt),
            "json" => Some(Self::Json),
            "docx" => Some(Self::Docx),
            _ => None,
        }
    }
}

/// Where a transcript to view or export comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
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
    /// Label speakers with the local diarizer (needs the speaker model).
    #[serde(default)]
    pub label_speakers: bool,
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
    /// Lemonade Server: connection refused, DNS failure, reset before any answer.
    ServerUnreachable,
    /// Lemonade Server: HTTP 401/403.
    ServerUnauthorized,
    /// Lemonade Server does not know the requested model.
    ServerModelMissing,
    Internal,
    /// Speaker labels: the recording at the stored path is not the one that
    /// was transcribed (another size, or modified since).
    SourceChanged,
    /// Speaker labels: the diarizer failed or its model is missing.
    SpeakerLabelsFailed,
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
    /// The engine the job runs on (fixed when it starts; settings' value while queued).
    pub engine: FileTranscriptionEngine,
    /// None when speaker labels were not requested.
    pub speakers: Option<SpeakerJobState>,
    pub error: Option<FileJobError>,
    /// get_file_transcript(Job) returns something (final, partial or running).
    pub has_transcript: bool,
    pub transcript_status: Option<TranscriptStatus>,
    pub history_id: Option<i64>,
    /// Unix milliseconds.
    pub enqueued_at: i64,
    /// Set for a speaker-label job: the transcript it labels. Such a job
    /// transcribes nothing and never has a transcript of its own.
    pub labels_for: Option<TranscriptSource>,
    /// Runs on dictation's loaded model; switching the dictation model stops it.
    #[serde(default)]
    pub shares_dictation_model: bool,
    /// Unix milliseconds when the job left the queue; None while queued.
    pub started_at: Option<i64>,
    /// Unix milliseconds when the job finished.
    pub finished_at: Option<i64>,
    /// Set once a transcription job's audio is all transcribed and it only
    /// waits for its speaker labels: the end of the last diarized window (0
    /// before the first), which never goes back. None until then.
    pub speakers_processed_ms: Option<u64>,
    /// The file the transcript was saved as automatically (Transcribe page
    /// setting), when it was. Later speaker edits do not change it.
    pub saved_file: Option<SavedTranscriptFile>,
    /// English reason the transcript could not be saved automatically. The
    /// job itself did not fail.
    pub save_error: Option<String>,
}

/// A transcript Handy saved as a file automatically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SavedTranscriptFile {
    /// The file written: a new file, never one that existed before.
    pub path: String,
    /// The folder it should have gone to (next to the recording, or the
    /// chosen one) when that could not be written, or was the temporary
    /// folder (which is emptied later), so it went to Documents/Handy
    /// Transcripts instead.
    pub unwritable_folder: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileTranscriptionEvent {
    /// Full job state after any change. Progress-only changes are throttled
    /// to at most one per 250 ms per job; status changes and speaker
    /// progress are never throttled.
    Updated { job: Box<FileJobSnapshot> },
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
    /// Number of labelled speakers; 0 when the transcript has none.
    pub speaker_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn segment_without_speaker_has_no_speaker_key() {
        let segment = TranscriptSegment::new(0, 1_500, "Hello.");
        assert_eq!(
            serde_json::to_value(&segment).expect("serialize"),
            json!({"start_ms": 0, "end_ms": 1_500, "text": "Hello."})
        );

        let labelled = TranscriptSegment {
            speaker: Some(2),
            ..segment.clone()
        };
        let value = serde_json::to_value(&labelled).expect("serialize");
        assert_eq!(value["speaker"], json!(2));
        let back: TranscriptSegment = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, labelled);

        let legacy: TranscriptSegment =
            serde_json::from_value(json!({"start_ms": 0, "end_ms": 1_500, "text": "Hello."}))
                .expect("deserialize legacy");
        assert_eq!(legacy, segment);
    }

    #[test]
    fn options_without_label_speakers_default_to_off() {
        let options: FileTranscriptionOptions =
            serde_json::from_value(json!({"save_to_history": true})).expect("deserialize");
        assert!(options.save_to_history);
        assert!(!options.label_speakers);
    }

    #[test]
    fn speaker_job_state_is_tagged_by_state() {
        let cases = [
            (SpeakerJobState::Pending, json!({"state": "pending"})),
            (SpeakerJobState::Running, json!({"state": "running"})),
            (
                SpeakerJobState::Done { speakers: 2 },
                json!({"state": "done", "speakers": 2}),
            ),
            (
                SpeakerJobState::Failed {
                    message: "boom".to_string(),
                },
                json!({"state": "failed", "message": "boom"}),
            ),
            (
                SpeakerJobState::Skipped {
                    reason: SpeakerSkipReason::ModelMissing,
                },
                json!({"state": "skipped", "reason": "model_missing"}),
            ),
            (
                SpeakerJobState::Skipped {
                    reason: SpeakerSkipReason::NoSpeech,
                },
                json!({"state": "skipped", "reason": "no_speech"}),
            ),
        ];
        for (state, expected) in cases {
            assert_eq!(serde_json::to_value(&state).expect("serialize"), expected);
        }
    }

    #[test]
    fn snapshot_serializes_labels_for_as_null_or_a_tagged_source() {
        let snapshot = FileJobSnapshot {
            job_id: 3,
            path: "/audio/talk.m4a".to_string(),
            file_name: "talk.m4a".to_string(),
            status: FileJobStatus::Queued,
            duration_ms: None,
            processed_ms: 0,
            chunks_done: 0,
            decoder: None,
            model_id: None,
            engine: FileTranscriptionEngine::Handy,
            speakers: None,
            error: None,
            has_transcript: false,
            transcript_status: None,
            history_id: None,
            enqueued_at: 1,
            labels_for: None,
            shares_dictation_model: true,
            started_at: None,
            finished_at: None,
            speakers_processed_ms: None,
            saved_file: None,
            save_error: None,
        };
        let value = serde_json::to_value(&snapshot).expect("serialize");
        assert_eq!(value["labels_for"], serde_json::Value::Null);
        assert_eq!(value["shares_dictation_model"], json!(true));
        assert_eq!(value["saved_file"], serde_json::Value::Null);

        let saved = FileJobSnapshot {
            started_at: Some(10),
            saved_file: Some(SavedTranscriptFile {
                path: "/audio/talk.docx".to_string(),
                unwritable_folder: None,
            }),
            ..snapshot.clone()
        };
        let value = serde_json::to_value(&saved).expect("serialize");
        assert_eq!(value["started_at"], json!(10));
        assert_eq!(
            value["saved_file"],
            json!({"path": "/audio/talk.docx", "unwritable_folder": null})
        );

        let label_job = FileJobSnapshot {
            labels_for: Some(TranscriptSource::History { history_id: 7 }),
            ..snapshot.clone()
        };
        let value = serde_json::to_value(&label_job).expect("serialize");
        assert_eq!(
            value["labels_for"],
            json!({"kind": "history", "history_id": 7})
        );
        let job = FileJobSnapshot {
            labels_for: Some(TranscriptSource::Job { job_id: 2 }),
            ..snapshot
        };
        let value = serde_json::to_value(&job).expect("serialize");
        assert_eq!(value["labels_for"], json!({"kind": "job", "job_id": 2}));
    }

    #[test]
    fn new_error_codes_are_snake_case() {
        assert_eq!(
            serde_json::to_value(FileJobErrorCode::SourceChanged).expect("serialize"),
            json!("source_changed")
        );
        assert_eq!(
            serde_json::to_value(FileJobErrorCode::SpeakerLabelsFailed).expect("serialize"),
            json!("speaker_labels_failed")
        );
    }
}
