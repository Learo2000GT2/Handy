//! Speaker labels: merges diarizer turns onto a transcript's segments,
//! rebuilds paragraphs as speaker turns and renders labelled text.
//!
//! Contract stub: the merge lands with the speaker labels work.
#![allow(dead_code, unused_variables, unused_imports)]

use crate::file_transcription::types::{
    is_no_space_language, FileTranscript, TranscriptSegment, TranscriptSpeakers,
};

/// One diarizer turn; `speaker` is the raw diarizer id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeakerTurn {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergeOptions {
    /// Join a speaker's turns closer than this.
    pub gap_fill_ms: u64,
    /// Unassigned pieces take a turn this close.
    pub nearest_ms: u64,
    /// Longer segments are split into sentences first.
    pub long_segment_ms: u64,
    /// Each side of a speaker-change split needs this much turn overlap.
    pub split_min_ms: u64,
    /// A monologue gets a new paragraph after this (at a sentence end).
    pub max_paragraph_ms: u64,
    /// `is_no_space_language(transcript.language)`.
    pub no_space: bool,
}

impl MergeOptions {
    pub fn for_transcript(t: &FileTranscript) -> Self {
        Self {
            gap_fill_ms: 300,
            nearest_ms: 1_000,
            long_segment_ms: 10_000,
            split_min_ms: 1_500,
            max_paragraph_ms: 60_000,
            no_space: is_no_space_language(t.language.as_deref()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MergeStats {
    pub speakers: u32,
    pub unassigned: u32,
    pub split: u32,
}

/// Sorts, drops empty/invalid turns, merges each speaker's turns whose gap ≤ gap_fill_ms.
pub fn normalize_turns(raw: &[SpeakerTurn], gap_fill_ms: u64) -> Vec<SpeakerTurn> {
    unimplemented!("speaker turn normalization")
}

/// Labels `t.segments`, rebuilds `t.paragraphs` as speaker turns and `t.text`
/// with labels, and sets `t.speakers`. With no turns (or no assigned segment)
/// leaves `t` unchanged and returns speakers == 0.
pub fn apply_speakers(
    t: &mut FileTranscript,
    turns: &[SpeakerTurn],
    model: &str,
    opts: &MergeOptions,
) -> MergeStats {
    MergeStats::default()
}

/// "Doug" or "Speaker 2" (English default, used in text and exports).
pub fn speaker_label(speakers: &TranscriptSpeakers, id: u32) -> String {
    unimplemented!("speaker label")
}

/// paragraphs → text: "Label: text" for paragraphs with a speaker, plain otherwise, joined by "\n\n".
pub fn render_text(
    paragraphs: &[TranscriptSegment],
    speakers: Option<&TranscriptSpeakers>,
) -> String {
    unimplemented!("labelled transcript text")
}

/// Trims; "" → None (back to "Speaker N"); max 60 chars; no control characters
/// or newlines; ':' allowed. Err(English message).
pub fn validate_speaker_name(name: Option<&str>) -> Result<Option<String>, String> {
    Err("not implemented".to_string())
}

/// Sets the name of speaker `id` and re-renders `t.text`. Err if `t` has no speakers or no such id.
pub fn rename_speaker(t: &mut FileTranscript, id: u32, name: Option<&str>) -> Result<(), String> {
    Err("not implemented".to_string())
}
