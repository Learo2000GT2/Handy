//! The live transcript shown during a meeting. It is a preview: the final
//! transcript is made from the saved audio after the meeting. Audio reaches
//! it through a bounded queue that the recording never waits on; when
//! transcription falls behind, audio is skipped for the preview only.

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::chunker::{AudioChunk, ChunkVerdict, SilenceChunker, SpeechGate};
use crate::file_transcription::types::TranscriptSegment;
use crate::managers::recordings_engine::FileModelSlot;
use crate::managers::transcription::{ChunkError, FileJobLease, TranscriptionManager};
use crate::settings::get_settings;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// Longest live chunk. Shorter chunks show text sooner but cost more engine
/// time per minute (Whisper encodes a 30 s window per call).
const LIVE_CHUNK_MS: u32 = 12_000;
const SAMPLES_PER_MS: u64 = 16;
/// Failed chunks in a row before the preview reports itself unavailable.
const FAILURES_BEFORE_UNAVAILABLE: u32 = 3;

/// Audio for the live transcript, with its position in the saved recording.
pub struct LiveFrame {
    pub samples: Vec<f32>,
    pub at_sample: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveStatus {
    /// Turned off for this meeting, or no model is selected.
    Off,
    /// Loading the model.
    Starting,
    /// Transcribing as the meeting goes.
    Listening,
    /// The preview cannot run; the recording is unaffected.
    Unavailable { message: String },
}

pub enum LiveEvent {
    Status(LiveStatus),
    Segments(Vec<TranscriptSegment>),
}

/// Lines speech models invent from silence or noise. Only dropped when they
/// are the whole of a chunk's text.
const PHANTOM_LINES: &[&str] = &["thank you", "thanks for watching", "you", "bye"];

fn is_phantom(text: &str) -> bool {
    let normalized: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.is_empty() || PHANTOM_LINES.contains(&normalized.as_str())
}

fn begin(
    tm: &TranscriptionManager,
    model_id: &str,
    token: &WorkToken,
) -> anyhow::Result<FileJobLease> {
    // Dictation is off during a meeting, so its engine and loaded model are
    // free for the preview and fastest.
    tm.begin_file_job(model_id, None, token.clone(), FileModelSlot::Dictation)
}

/// Runs the live transcript until `frames` closes or `token` is cancelled.
pub fn run(
    app: AppHandle,
    frames: Receiver<LiveFrame>,
    token: WorkToken,
    on_event: impl Fn(LiveEvent),
) {
    let tm: Arc<TranscriptionManager> = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
    let model_id = get_settings(&app).selected_model;
    if model_id.trim().is_empty() {
        on_event(LiveEvent::Status(LiveStatus::Off));
        // Keep draining so the queue never fills.
        for _ in frames.iter() {}
        return;
    }

    on_event(LiveEvent::Status(LiveStatus::Starting));
    let mut lease = match begin(&tm, &model_id, &token) {
        Ok(lease) => lease,
        Err(error) => {
            log::warn!("Live meeting transcript unavailable: {:#}", error);
            on_event(LiveEvent::Status(LiveStatus::Unavailable {
                message: format!("{:#}", error),
            }));
            for _ in frames.iter() {}
            return;
        }
    };
    on_event(LiveEvent::Status(LiveStatus::Listening));

    let chunk_ms = LIVE_CHUNK_MS.min(lease.limits().max_chunk_ms).max(5_000);
    let new_chunker = || SilenceChunker::new(chunk_ms, chunk_ms / 2);
    let mut chunker = new_chunker();
    let mut gate = SpeechGate::new(true).ok();
    // Saved-audio position of the chunker's first sample, and of the sample
    // expected next. Skipped audio starts a new chunker at its position.
    let mut base_sample = 0u64;
    let mut next_sample: Option<u64> = None;
    let mut failures = 0u32;
    let mut ready: Vec<AudioChunk> = Vec::new();

    let mut transcribe = |chunk: AudioChunk, base: u64, lease: &mut FileJobLease| -> bool {
        let audio = match gate.as_mut().map(|gate| gate.prepare(&chunk.samples)) {
            Some(ChunkVerdict::SkipSilent) => return true,
            Some(ChunkVerdict::Transcribe(audio)) => audio,
            None => chunk.samples.clone(),
        };
        let offset_ms = (base + chunk.start_sample) / SAMPLES_PER_MS;
        let mut model_changed_once = false;
        loop {
            if token.is_cancelled() {
                return false;
            }
            match lease.transcribe_chunk(&audio, offset_ms, chunk.len_ms()) {
                Ok(result) => {
                    if failures >= FAILURES_BEFORE_UNAVAILABLE {
                        on_event(LiveEvent::Status(LiveStatus::Listening));
                    }
                    failures = 0;
                    if !is_phantom(&result.text) && !result.segments.is_empty() {
                        on_event(LiveEvent::Segments(result.segments));
                    }
                    return true;
                }
                Err(ChunkError::Yield) => std::thread::sleep(Duration::from_millis(150)),
                Err(ChunkError::Cancelled) => return false,
                Err(ChunkError::ModelChanged { current_model, .. }) if !model_changed_once => {
                    model_changed_once = true;
                    match begin(&tm, &current_model, &token) {
                        Ok(next) => *lease = next,
                        Err(error) => {
                            on_event(LiveEvent::Status(LiveStatus::Unavailable {
                                message: format!("{:#}", error),
                            }));
                            return true;
                        }
                    }
                }
                Err(error) => {
                    failures += 1;
                    log::warn!("Live meeting chunk failed: {}", error);
                    if failures == FAILURES_BEFORE_UNAVAILABLE {
                        on_event(LiveEvent::Status(LiveStatus::Unavailable {
                            message: error.to_string(),
                        }));
                    }
                    return true;
                }
            }
        }
    };

    for frame in frames.iter() {
        if token.is_cancelled() {
            break;
        }
        if next_sample.is_some_and(|expected| expected != frame.at_sample) {
            // The preview fell behind and audio was skipped: finish what is
            // buffered and continue from the new position.
            let _ = chunker.finish(&mut |chunk| {
                ready.push(chunk);
                Ok(())
            });
            for chunk in ready.drain(..) {
                if !transcribe(chunk, base_sample, &mut lease) {
                    return;
                }
            }
            chunker = new_chunker();
            base_sample = frame.at_sample;
        } else if next_sample.is_none() {
            base_sample = frame.at_sample;
        }
        next_sample = Some(frame.at_sample + frame.samples.len() as u64);

        let _ = chunker.push(&frame.samples, &mut |chunk| {
            ready.push(chunk);
            Ok(())
        });
        for chunk in ready.drain(..) {
            if !transcribe(chunk, base_sample, &mut lease) {
                return;
            }
        }
    }
    // Audio still buffered at the end is left to the final transcript.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phantom_lines_are_recognized() {
        assert!(is_phantom("Thank you."));
        assert!(is_phantom("  you "));
        assert!(is_phantom("..."));
        assert!(!is_phantom("Thank you for coming in today."));
        assert!(!is_phantom("Mr. Lee starts Monday."));
    }
}
