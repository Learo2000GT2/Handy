//! Speaker labels: merges diarizer turns onto a transcript's segments,
//! rebuilds paragraphs as speaker turns and renders labelled text.
//!
//! Everything here is pure; the diarizer itself runs in `diarize`. The
//! engines give no word timestamps, so a segment that spans a speaker change
//! is cut at the word nearest the change, in proportion to its text.

use crate::file_transcription::export::is_combining;
use crate::file_transcription::types::{
    is_no_space_language, FileTranscript, TimingGrain, TranscriptSegment, TranscriptSpeaker,
    TranscriptSpeakers,
};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

/// Longest speaker name, in `char`s.
pub const MAX_SPEAKER_NAME_CHARS: usize = 60;
/// A piece is split at most this many times in a row, which covers three
/// speakers in one 28 s chunk.
const MAX_SPLIT_DEPTH: u32 = 2;

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
    let mut valid: Vec<SpeakerTurn> = raw
        .iter()
        .copied()
        .filter(|turn| turn.end_ms > turn.start_ms)
        .collect();
    valid.sort_by_key(|turn| (turn.speaker, turn.start_ms, turn.end_ms));

    let mut merged: Vec<SpeakerTurn> = Vec::with_capacity(valid.len());
    for turn in valid {
        match merged.last_mut() {
            Some(last)
                if last.speaker == turn.speaker
                    && turn.start_ms <= last.end_ms.saturating_add(gap_fill_ms) =>
            {
                last.end_ms = last.end_ms.max(turn.end_ms);
            }
            _ => merged.push(turn),
        }
    }
    merged.sort_by_key(|turn| (turn.start_ms, turn.end_ms, turn.speaker));
    merged
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
    let turns = normalize_turns(turns, opts.gap_fill_ms);
    if turns.is_empty() {
        return MergeStats::default();
    }
    let index = TurnIndex::new(&turns);
    // A transcript stored without segments is timed by its paragraphs.
    let source = if t.segments.is_empty() {
        &t.paragraphs
    } else {
        &t.segments
    };

    let mut stats = MergeStats::default();
    let mut pieces: Vec<Piece> = Vec::new();
    for (origin, segment) in source.iter().enumerate() {
        for piece in initial_pieces(origin, segment, t.timing, opts) {
            let prev = pieces.last().and_then(|p| p.speaker);
            index.label(piece, &segment.text, prev, 0, opts, &mut pieces, &mut stats);
        }
    }
    let labelled = coalesce(source, pieces);

    // Display ids 1..n in order of first appearance in the text.
    let mut display: HashMap<u32, u32> = HashMap::new();
    let mut segments = Vec::with_capacity(labelled.len());
    for mut segment in labelled {
        segment.speaker = segment.speaker.map(|raw| {
            let next = display.len() as u32 + 1;
            *display.entry(raw).or_insert(next)
        });
        segments.push(segment);
    }
    let unassigned = segments.iter().filter(|s| s.speaker.is_none()).count() as u32;
    if display.is_empty() {
        return MergeStats {
            unassigned,
            ..MergeStats::default()
        };
    }

    let speakers = TranscriptSpeakers {
        speakers: (1..=display.len() as u32)
            .map(|id| TranscriptSpeaker { id, name: None })
            .collect(),
        approximate: t.timing == TimingGrain::Chunk,
        unassigned_segments: unassigned,
        model: model.to_string(),
    };
    t.paragraphs = speaker_paragraphs(&segments, opts);
    t.segments = segments;
    t.text = render_text(&t.paragraphs, Some(&speakers));
    t.speakers = Some(speakers);

    stats.speakers = display.len() as u32;
    stats.unassigned = unassigned;
    stats
}

/// "Doug" or "Speaker 2" (English default, used in text and exports).
pub fn speaker_label(speakers: &TranscriptSpeakers, id: u32) -> String {
    speakers
        .speakers
        .iter()
        .find(|speaker| speaker.id == id)
        .and_then(|speaker| speaker.name.as_deref())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("Speaker {id}"), str::to_string)
}

/// paragraphs → text: "Label: text" for paragraphs with a speaker, plain otherwise, joined by "\n\n".
pub fn render_text(
    paragraphs: &[TranscriptSegment],
    speakers: Option<&TranscriptSpeakers>,
) -> String {
    paragraphs
        .iter()
        .map(|paragraph| match (paragraph.speaker, speakers) {
            (Some(id), Some(speakers)) => {
                format!("{}: {}", speaker_label(speakers, id), paragraph.text)
            }
            _ => paragraph.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Trims; "" → None (back to "Speaker N"); max 60 chars; no control characters
/// or newlines; ':' allowed. Err(English message).
pub fn validate_speaker_name(name: Option<&str>) -> Result<Option<String>, String> {
    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_SPEAKER_NAME_CHARS {
        return Err(format!(
            "A speaker name can have at most {MAX_SPEAKER_NAME_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("A speaker name can't contain line breaks or control characters".to_string());
    }
    Ok(Some(name.to_string()))
}

/// Sets the name of speaker `id` and re-renders `t.text`. Err if `t` has no speakers or no such id.
pub fn rename_speaker(t: &mut FileTranscript, id: u32, name: Option<&str>) -> Result<(), String> {
    let name = validate_speaker_name(name)?;
    let speakers = t
        .speakers
        .as_mut()
        .ok_or_else(|| "This transcript has no speaker labels".to_string())?;
    let speaker = speakers
        .speakers
        .iter_mut()
        .find(|speaker| speaker.id == id)
        .ok_or_else(|| format!("This transcript has no speaker {id}"))?;
    speaker.name = name;
    t.text = render_text(&t.paragraphs, t.speakers.as_ref());
    Ok(())
}

/// A part of one source segment: a byte range of its text and the time it
/// is thought to be spoken.
#[derive(Clone, Debug, PartialEq)]
struct Piece {
    /// Index of the source segment.
    origin: usize,
    text: Range<usize>,
    start_ms: u64,
    end_ms: u64,
    /// Raw diarizer id.
    speaker: Option<u32>,
}

/// The segment as one piece, or as sentence pieces with times interpolated
/// by character count when it is chunk-grained or long.
fn initial_pieces(
    origin: usize,
    segment: &TranscriptSegment,
    timing: TimingGrain,
    opts: &MergeOptions,
) -> Vec<Piece> {
    let whole = Piece {
        origin,
        text: 0..segment.text.len(),
        start_ms: segment.start_ms,
        end_ms: segment.end_ms,
        speaker: None,
    };
    let duration = segment.end_ms.saturating_sub(segment.start_ms);
    if timing != TimingGrain::Chunk && duration <= opts.long_segment_ms {
        return vec![whole];
    }
    let spans = sentence_spans(&segment.text, opts.no_space);
    if spans.len() < 2 {
        return vec![whole];
    }
    let total = segment.text.chars().count();
    let at = |byte: usize| interpolate(segment, segment.text[..byte].chars().count(), total);
    spans
        .iter()
        .enumerate()
        .map(|(i, span)| Piece {
            origin,
            text: span.clone(),
            start_ms: if i == 0 {
                segment.start_ms
            } else {
                at(span.start)
            },
            end_ms: spans
                .get(i + 1)
                .map_or(segment.end_ms, |next| at(next.start)),
            speaker: None,
        })
        .collect()
}

/// The time `chars` characters into `segment`, by linear interpolation.
fn interpolate(segment: &TranscriptSegment, chars: usize, total: usize) -> u64 {
    let duration = u128::from(segment.end_ms.saturating_sub(segment.start_ms));
    let offset = duration * chars as u128 / total.max(1) as u128;
    segment.start_ms + offset as u64
}

/// Byte ranges of the sentences of `text`, trimmed, covering all of its
/// non-blank content. A sentence ends after `. ? ! …` (or their full-width
/// forms), and any closing quotes or brackets, followed by whitespace or the
/// end; in a language written without spaces `。？！` end one directly.
fn sentence_spans(text: &str, no_space: bool) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !is_sentence_end(c) {
            continue;
        }
        let mut end = i + c.len_utf8();
        let mut cjk_end = no_space && is_full_width_end(c);
        while let Some(&(j, next)) = chars.peek() {
            if !(is_sentence_end(next) || is_closing(next)) {
                break;
            }
            cjk_end |= no_space && is_full_width_end(next);
            end = j + next.len_utf8();
            chars.next();
        }
        let at_break = chars.peek().is_none_or(|&(_, next)| next.is_whitespace());
        if at_break || cjk_end {
            push_trimmed(&mut spans, text, start..end);
            start = end;
        }
    }
    push_trimmed(&mut spans, text, start..text.len());
    spans
}

fn push_trimmed(spans: &mut Vec<Range<usize>>, text: &str, range: Range<usize>) {
    let slice = &text[range.clone()];
    let leading = slice.len() - slice.trim_start().len();
    let trimmed = slice.trim();
    if !trimmed.is_empty() {
        let start = range.start + leading;
        spans.push(start..start + trimmed.len());
    }
}

fn is_sentence_end(c: char) -> bool {
    matches!(c, '.' | '?' | '!' | '…' | '。' | '？' | '！')
}

fn is_full_width_end(c: char) -> bool {
    matches!(c, '。' | '？' | '！')
}

fn is_closing(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | ')' | ']' | '”' | '’' | '」' | '』' | '）' | '》' | '»'
    )
}

/// Whether `text` ends a sentence (ignoring closing quotes and brackets).
fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .chars()
        .rev()
        .find(|&c| !is_closing(c))
        .is_some_and(is_sentence_end)
}

/// Punctuation after which text written without spaces may be cut.
fn is_cut_punctuation(c: char) -> bool {
    matches!(
        c,
        '，' | '、' | '；' | '：' | '。' | '？' | '！' | ',' | ';' | ':' | '.' | '?' | '!' | '…'
    )
}

/// Normalized turns grouped by raw speaker: each list sorted and disjoint,
/// so the turns of a speaker in a time range are found by binary search.
struct TurnIndex {
    by_speaker: BTreeMap<u32, Vec<(u64, u64)>>,
    /// Start of each speaker's first turn, for tie-breaks.
    first_start: HashMap<u32, u64>,
}

/// The speaker a piece gets, and every speaker's overlap with it.
struct Choice {
    speaker: Option<u32>,
    /// (raw id, overlap ms) of every speaker overlapping the piece, by id.
    overlaps: Vec<(u32, u64)>,
}

impl TurnIndex {
    fn new(turns: &[SpeakerTurn]) -> Self {
        let mut by_speaker: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
        let mut first_start = HashMap::new();
        for turn in turns {
            by_speaker
                .entry(turn.speaker)
                .or_default()
                .push((turn.start_ms, turn.end_ms));
            first_start.entry(turn.speaker).or_insert(turn.start_ms);
        }
        Self {
            by_speaker,
            first_start,
        }
    }

    /// `speaker`'s turns clipped to `start..end`.
    fn inside(&self, speaker: u32, start: u64, end: u64) -> Vec<(u64, u64)> {
        let Some(turns) = self.by_speaker.get(&speaker) else {
            return Vec::new();
        };
        let first = turns.partition_point(|&(_, turn_end)| turn_end <= start);
        turns[first..]
            .iter()
            .take_while(|&&(turn_start, _)| turn_start < end)
            .map(|&(turn_start, turn_end)| (turn_start.max(start), turn_end.min(end)))
            .filter(|(s, e)| e > s)
            .collect()
    }

    fn overlap(&self, speaker: u32, start: u64, end: u64) -> u64 {
        self.inside(speaker, start, end)
            .iter()
            .map(|(s, e)| e - s)
            .sum()
    }

    /// Distance from `start..end` to `speaker`'s nearest turn (0 when they overlap).
    fn distance(&self, speaker: u32, start: u64, end: u64) -> Option<u64> {
        let turns = self.by_speaker.get(&speaker)?;
        let next = turns.partition_point(|&(_, turn_end)| turn_end <= start);
        let after = turns
            .get(next)
            .map(|&(turn_start, _)| turn_start.saturating_sub(end));
        let before = next
            .checked_sub(1)
            .and_then(|i| turns.get(i))
            .map(|&(_, turn_end)| start.saturating_sub(turn_end));
        match (before, after) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Most overlap wins; a tie goes to `prev` when it is among the tied,
    /// otherwise to the speaker who spoke first. Without overlap, the
    /// nearest turn within `nearest_ms`, tied the same way.
    fn choose(&self, start: u64, end: u64, prev: Option<u32>, nearest_ms: u64) -> Choice {
        let overlaps: Vec<(u32, u64)> = self
            .by_speaker
            .keys()
            .map(|&speaker| (speaker, self.overlap(speaker, start, end)))
            .filter(|&(_, ms)| ms > 0)
            .collect();
        let speaker = if overlaps.is_empty() {
            let near: Vec<(u32, u64)> = self
                .by_speaker
                .keys()
                .filter_map(|&speaker| Some((speaker, self.distance(speaker, start, end)?)))
                .filter(|&(_, distance)| distance <= nearest_ms)
                .collect();
            near.iter()
                .map(|&(_, distance)| distance)
                .min()
                .and_then(|best| self.break_tie(&near, |distance| distance == best, prev))
        } else {
            let best = overlaps.iter().map(|&(_, ms)| ms).max().unwrap_or(0);
            self.break_tie(&overlaps, |ms| ms == best, prev)
        };
        Choice { speaker, overlaps }
    }

    /// Among the speakers whose score `is_best`: `prev` if it is one of
    /// them, else the one who spoke first.
    fn break_tie(
        &self,
        scored: &[(u32, u64)],
        is_best: impl Fn(u64) -> bool,
        prev: Option<u32>,
    ) -> Option<u32> {
        let tied = scored
            .iter()
            .filter(|&&(_, score)| is_best(score))
            .map(|&(speaker, _)| speaker);
        if let Some(prev) = prev.filter(|prev| tied.clone().any(|s| s == *prev)) {
            return Some(prev);
        }
        tied.min_by_key(|speaker| (self.first_start.get(speaker).copied(), *speaker))
    }

    /// Labels `piece`, first splitting it where the speaker changes inside
    /// it (at most `MAX_SPLIT_DEPTH` deep), and appends the result to `out`.
    #[allow(clippy::too_many_arguments)]
    fn label(
        &self,
        mut piece: Piece,
        text: &str,
        prev: Option<u32>,
        depth: u32,
        opts: &MergeOptions,
        out: &mut Vec<Piece>,
        stats: &mut MergeStats,
    ) {
        let choice = self.choose(piece.start_ms, piece.end_ms, prev, opts.nearest_ms);
        piece.speaker = choice.speaker;
        if depth < MAX_SPLIT_DEPTH {
            if let Some((left, right)) = self.split(&piece, &choice, text, opts) {
                stats.split += 1;
                self.label(left, text, prev, depth + 1, opts, out, stats);
                let prev = out.last().and_then(|p| p.speaker);
                self.label(right, text, prev, depth + 1, opts, out, stats);
                return;
            }
        }
        out.push(piece);
    }

    /// Splits `piece` in two where its best speaker and the runner-up take
    /// turns: each side needs `split_min_ms` of its speaker, and the two must
    /// mostly talk one after the other rather than at once. The cut is at
    /// the turn edge that agrees best with the turns, in either order.
    fn split(
        &self,
        piece: &Piece,
        choice: &Choice,
        text: &str,
        opts: &MergeOptions,
    ) -> Option<(Piece, Piece)> {
        let (start, end) = (piece.start_ms, piece.end_ms);
        if end.saturating_sub(start) < 2 * opts.split_min_ms {
            return None;
        }
        let best = piece.speaker?;
        let best_ov = choice
            .overlaps
            .iter()
            .find(|&&(speaker, _)| speaker == best)
            .map_or(0, |&(_, ms)| ms);
        let (runner_up, runner_ov) = choice
            .overlaps
            .iter()
            .filter(|&&(speaker, _)| speaker != best)
            .max_by_key(|&&(speaker, ms)| (ms, std::cmp::Reverse(speaker)))
            .copied()?;
        if runner_ov < opts.split_min_ms {
            return None;
        }
        let a_turns = self.inside(best, start, end);
        let b_turns = self.inside(runner_up, start, end);
        if intersection_ms(&a_turns, &b_turns) * 2 >= best_ov.min(runner_ov) {
            // Overlapping speech, not a change of speaker.
            return None;
        }

        let mut cut: Option<(u64, u64)> = None;
        let edges = a_turns
            .iter()
            .chain(&b_turns)
            .flat_map(|&(s, e)| [s, e])
            .filter(|&edge| edge > start && edge < end);
        for edge in edges {
            for (first, second) in [(best, runner_up), (runner_up, best)] {
                let left = self.overlap(first, start, edge);
                let right = self.overlap(second, edge, end);
                if left < opts.split_min_ms || right < opts.split_min_ms {
                    continue;
                }
                if cut.is_none_or(|(_, score)| left + right > score) {
                    cut = Some((edge, left + right));
                }
            }
        }
        let (cut_ms, _) = cut?;

        let fraction = (cut_ms - start) as f64 / (end - start) as f64;
        let gap = cut_text(text, piece.text.clone(), fraction, opts.no_space)?;
        let left = Piece {
            text: piece.text.start..gap.start,
            end_ms: cut_ms,
            speaker: None,
            ..piece.clone()
        };
        let right = Piece {
            text: gap.end..piece.text.end,
            start_ms: cut_ms,
            speaker: None,
            ..piece.clone()
        };
        Some((left, right))
    }
}

/// Total time two sorted, disjoint interval lists share.
fn intersection_ms(a: &[(u64, u64)], b: &[(u64, u64)]) -> u64 {
    let (mut i, mut j, mut total) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        total += a[i].1.min(b[j].1).saturating_sub(a[i].0.max(b[j].0));
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    total
}

/// Where to cut `text[range]` so the left part holds about `fraction` of its
/// characters: the byte range to drop between two non-blank parts (empty
/// when nothing is dropped). At a word boundary; in a language written
/// without spaces also after punctuation, or between any two characters
/// when it has neither. None when the text can't be cut.
fn cut_text(
    text: &str,
    range: Range<usize>,
    fraction: f64,
    no_space: bool,
) -> Option<Range<usize>> {
    let slice = &text[range.clone()];
    let chars: Vec<(usize, char)> = slice.char_indices().collect();
    let target = (chars.len() as f64 * fraction).round() as usize;

    // (char index of the cut, byte range to drop within `slice`)
    let mut candidates: Vec<(usize, Range<usize>)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (byte, c) = chars[i];
        if c.is_whitespace() {
            let first = i;
            while i < chars.len() && chars[i].1.is_whitespace() {
                i += 1;
            }
            let gap_end = chars.get(i).map_or(slice.len(), |&(b, _)| b);
            candidates.push((first, byte..gap_end));
            continue;
        }
        let followed_by_space = chars.get(i + 1).is_some_and(|&(_, n)| n.is_whitespace());
        if no_space && is_cut_punctuation(c) && !followed_by_space {
            let after = byte + c.len_utf8();
            candidates.push((i + 1, after..after));
        }
        i += 1;
    }
    if no_space && candidates.is_empty() {
        candidates = chars
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, &(_, c))| !is_combining(c))
            .map(|(index, &(byte, _))| (index, byte..byte))
            .collect();
    }
    candidates
        .into_iter()
        .filter(|(_, gap)| {
            !slice[..gap.start].trim().is_empty() && !slice[gap.end..].trim().is_empty()
        })
        .min_by_key(|(index, _)| index.abs_diff(target))
        .map(|(_, gap)| range.start + gap.start..range.start + gap.end)
}

/// Merges consecutive pieces of the same segment and speaker, and turns the
/// pieces back into segments. A segment that stayed whole keeps its exact
/// text and times; only its speaker is set.
fn coalesce(source: &[TranscriptSegment], pieces: Vec<Piece>) -> Vec<TranscriptSegment> {
    let mut merged: Vec<Piece> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match merged.last_mut() {
            Some(last) if last.origin == piece.origin && last.speaker == piece.speaker => {
                last.text.end = piece.text.end;
                last.end_ms = last.end_ms.max(piece.end_ms);
            }
            _ => merged.push(piece),
        }
    }

    let mut per_origin: HashMap<usize, usize> = HashMap::new();
    for piece in &merged {
        *per_origin.entry(piece.origin).or_default() += 1;
    }
    merged
        .into_iter()
        .map(|piece| {
            let segment = &source[piece.origin];
            if per_origin.get(&piece.origin) == Some(&1) {
                TranscriptSegment {
                    speaker: piece.speaker,
                    ..segment.clone()
                }
            } else {
                TranscriptSegment {
                    start_ms: piece.start_ms,
                    end_ms: piece.end_ms.max(piece.start_ms + 1),
                    text: segment.text[piece.text].trim().to_string(),
                    speaker: piece.speaker,
                }
            }
        })
        .collect()
}

/// Paragraphs as speaker turns: a new one when the speaker changes, or when
/// a monologue reaches `max_paragraph_ms` at a sentence end (twice that
/// anywhere). An unlabelled segment joins the paragraph it follows; a
/// paragraph takes the speaker of its first labelled segment.
fn speaker_paragraphs(
    segments: &[TranscriptSegment],
    opts: &MergeOptions,
) -> Vec<TranscriptSegment> {
    let separator = if opts.no_space { "" } else { " " };
    let mut paragraphs: Vec<TranscriptSegment> = Vec::new();
    let mut last_text: Option<&str> = None;
    for segment in segments {
        let continues = paragraphs.last().is_some_and(|current| {
            let span = current.end_ms.saturating_sub(current.start_ms);
            let speaker_changed = matches!(
                (current.speaker, segment.speaker),
                (Some(a), Some(b)) if a != b
            );
            let monologue_ends = span >= 2 * opts.max_paragraph_ms
                || (span >= opts.max_paragraph_ms && last_text.is_some_and(ends_sentence));
            !speaker_changed && !monologue_ends
        });
        match paragraphs.last_mut() {
            Some(current) if continues => {
                current.text.push_str(separator);
                current.text.push_str(&segment.text);
                current.end_ms = current.end_ms.max(segment.end_ms);
                current.speaker = current.speaker.or(segment.speaker);
            }
            _ => paragraphs.push(segment.clone()),
        }
        last_text = Some(&segment.text);
    }
    paragraphs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::types::TranscriptStatus;

    fn turn(start_ms: u64, end_ms: u64, speaker: u32) -> SpeakerTurn {
        SpeakerTurn {
            start_ms,
            end_ms,
            speaker,
        }
    }

    fn seg(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment::new(start_ms, end_ms, text)
    }

    fn labelled(start_ms: u64, end_ms: u64, text: &str, speaker: u32) -> TranscriptSegment {
        TranscriptSegment {
            speaker: Some(speaker),
            ..seg(start_ms, end_ms, text)
        }
    }

    /// A transcript whose paragraphs are its segments.
    fn transcript(timing: TimingGrain, segments: Vec<TranscriptSegment>) -> FileTranscript {
        FileTranscript {
            source_path: "/tmp/talk.wav".to_string(),
            source_name: "talk.wav".to_string(),
            duration_ms: segments.last().map_or(0, |s| s.end_ms),
            model_id: "small".to_string(),
            model_name: None,
            language: Some("en".to_string()),
            status: TranscriptStatus::Completed,
            status_detail: None,
            timing,
            text: segments
                .iter()
                .map(|s| s.text.clone())
                .collect::<Vec<_>>()
                .join("\n\n"),
            paragraphs: segments.clone(),
            segments,
            created_at: 0,
            history_id: None,
            speakers: None,
        }
    }

    fn apply(t: &mut FileTranscript, turns: &[SpeakerTurn]) -> MergeStats {
        let opts = MergeOptions::for_transcript(t);
        apply_speakers(t, turns, "test-diarizer", &opts)
    }

    fn speakers_of(segments: &[TranscriptSegment]) -> Vec<Option<u32>> {
        segments.iter().map(|s| s.speaker).collect()
    }

    #[test]
    fn normalize_sorts_merges_gaps_and_drops_invalid() {
        let raw = [
            turn(5_000, 6_000, 4),
            turn(0, 1_000, 1),
            turn(1_200, 2_000, 1), // 200 ms gap: merged
            turn(2_400, 3_000, 1), // 400 ms gap: kept apart
            turn(3_000, 3_000, 1), // empty
            turn(4_000, 3_500, 4), // inverted
            turn(5_500, 7_000, 4), // overlaps the previous turn of 4
            turn(1_500, 1_800, 4),
        ];
        assert_eq!(
            normalize_turns(&raw, 300),
            vec![
                turn(0, 2_000, 1),
                turn(1_500, 1_800, 4),
                turn(2_400, 3_000, 1),
                turn(5_000, 7_000, 4),
            ]
        );
        assert!(normalize_turns(&[], 300).is_empty());
    }

    #[test]
    fn two_speakers_alternate() {
        let mut t = transcript(
            TimingGrain::Segment,
            vec![
                seg(0, 3_000, "Hello there."),
                seg(3_000, 6_000, "How are you?"),
                seg(6_500, 9_000, "Fine, thanks."),
                seg(9_500, 12_000, "Good to hear."),
            ],
        );
        let stats = apply(
            &mut t,
            &[
                turn(0, 6_100, 1),
                turn(6_400, 9_100, 2),
                turn(9_400, 12_000, 1),
            ],
        );
        assert_eq!(
            stats,
            MergeStats {
                speakers: 2,
                unassigned: 0,
                split: 0
            }
        );
        assert_eq!(
            speakers_of(&t.segments),
            [Some(1), Some(1), Some(2), Some(1)]
        );
        assert_eq!(
            t.paragraphs,
            vec![
                labelled(0, 6_000, "Hello there. How are you?", 1),
                labelled(6_500, 9_000, "Fine, thanks.", 2),
                labelled(9_500, 12_000, "Good to hear.", 1),
            ]
        );
        assert_eq!(
            t.text,
            "Speaker 1: Hello there. How are you?\n\nSpeaker 2: Fine, thanks.\n\n\
             Speaker 1: Good to hear."
        );
        assert_eq!(
            t.speakers,
            Some(TranscriptSpeakers {
                speakers: vec![
                    TranscriptSpeaker { id: 1, name: None },
                    TranscriptSpeaker { id: 2, name: None }
                ],
                approximate: false,
                unassigned_segments: 0,
                model: "test-diarizer".to_string(),
            })
        );
    }

    #[test]
    fn sparse_raw_ids_are_renumbered_by_first_appearance() {
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 2_000, "First."), seg(2_000, 4_000, "Second.")],
        );
        // Raw 4 speaks first, then raw 1; raw 3 only laughs where nothing
        // was transcribed and gets no id.
        apply(
            &mut t,
            &[
                turn(0, 2_000, 4),
                turn(2_000, 4_000, 1),
                turn(9_000, 9_500, 3),
            ],
        );
        assert_eq!(speakers_of(&t.segments), [Some(1), Some(2)]);
        assert_eq!(t.speakers.map(|s| s.speakers.len()), Some(2));
    }

    #[test]
    fn segment_spanning_change_is_split_at_sentence() {
        // 14 s, so split into sentences (64 chars; the 2nd starts at char 33,
        // the 3rd at 41): the first sentence is raw speaker 7's, the rest 2's.
        let text = "I stand most of the day at work. Really? For how long each time?";
        let mut t = transcript(TimingGrain::Segment, vec![seg(10_000, 24_000, text)]);
        let stats = apply(&mut t, &[turn(10_000, 17_200, 7), turn(17_400, 24_000, 2)]);
        assert_eq!(stats.speakers, 2);
        assert_eq!(stats.split, 0);
        assert_eq!(
            t.segments,
            vec![
                labelled(10_000, 17_218, "I stand most of the day at work.", 1),
                labelled(17_218, 24_000, "Really? For how long each time?", 2),
            ]
        );
        assert_eq!(t.paragraphs, t.segments);
        assert!(!t.speakers.expect("speakers").approximate);
    }

    #[test]
    fn short_segment_spanning_change_is_cut_at_the_nearest_word() {
        // 8 s, so not split into sentences, but each side has 4 s of its
        // speaker: cut at the word nearest the middle of the text.
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 8_000, "one two three four five six seven eight")],
        );
        let stats = apply(&mut t, &[turn(0, 4_000, 1), turn(4_000, 8_000, 2)]);
        assert_eq!(stats.split, 1);
        assert_eq!(
            t.segments,
            vec![
                labelled(0, 4_000, "one two three four", 1),
                labelled(4_000, 8_000, "five six seven eight", 2),
            ]
        );
    }

    #[test]
    fn change_found_after_a_short_blip() {
        // B makes a short noise early on, then really takes over at 5 s.
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 8_000, "one two three four five six seven eight")],
        );
        apply(
            &mut t,
            &[turn(0, 5_000, 1), turn(300, 500, 2), turn(5_000, 8_000, 2)],
        );
        assert_eq!(
            t.segments,
            vec![
                labelled(0, 5_000, "one two three four five", 1),
                labelled(5_000, 8_000, "six seven eight", 2),
            ]
        );
    }

    #[test]
    fn three_speakers_in_one_chunk_split_twice() {
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 9_000, "aa bb cc dd ee ff gg hh ii")],
        );
        let stats = apply(
            &mut t,
            &[
                turn(0, 3_000, 1),
                turn(3_000, 6_000, 2),
                turn(6_000, 9_000, 3),
            ],
        );
        assert_eq!(stats.speakers, 3);
        assert_eq!(stats.split, 2);
        assert_eq!(
            t.segments,
            vec![
                labelled(0, 3_000, "aa bb cc", 1),
                labelled(3_000, 6_000, "dd ee ff", 2),
                labelled(6_000, 9_000, "gg hh ii", 3),
            ]
        );
    }

    #[test]
    fn split_needs_min_overlap_each_side() {
        // B speaks for only 1 s at the end of an 8 s segment: no split.
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 8_000, "one two three four five six seven eight")],
        );
        let stats = apply(&mut t, &[turn(0, 7_000, 1), turn(7_000, 8_000, 2)]);
        assert_eq!(stats.split, 0);
        assert_eq!(speakers_of(&t.segments), [Some(1)]);
        assert_eq!(
            t.segments[0].text,
            "one two three four five six seven eight"
        );

        // Nor a segment shorter than twice the minimum.
        let mut t = transcript(TimingGrain::Segment, vec![seg(0, 2_900, "one two three")]);
        assert_eq!(
            apply(&mut t, &[turn(0, 1_450, 1), turn(1_450, 2_900, 2)]).split,
            0
        );
    }

    #[test]
    fn simultaneous_speech_goes_to_larger_overlap() {
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 8_000, "we both talk at the same time here")],
        );
        // Both speak through most of it: not a change of speaker.
        let stats = apply(&mut t, &[turn(0, 8_000, 1), turn(1_000, 7_000, 2)]);
        assert_eq!(stats.split, 0);
        assert_eq!(speakers_of(&t.segments), [Some(1)]);
        assert_eq!(t.speakers.map(|s| s.speakers.len()), Some(1));
    }

    #[test]
    fn unassigned_takes_nearest_within_1s_else_none() {
        let mut t = transcript(
            TimingGrain::Segment,
            vec![
                seg(0, 2_000, "Spoken."),
                seg(2_500, 3_000, "Mumble."), // 500 ms after speaker 1's turn
                seg(6_000, 7_000, "Echo."),   // over 3 s from any turn
                seg(9_800, 10_000, "Breath."), // 200 ms before speaker 2's turn
                seg(10_000, 12_000, "Reply."),
            ],
        );
        let stats = apply(&mut t, &[turn(0, 2_000, 1), turn(10_200, 12_000, 2)]);
        assert_eq!(
            speakers_of(&t.segments),
            [Some(1), Some(1), None, Some(2), Some(2)]
        );
        assert_eq!(stats.unassigned, 1);
        assert_eq!(t.speakers.as_ref().map(|s| s.unassigned_segments), Some(1));
        // The unlabelled segment stays in the paragraph it follows.
        assert_eq!(
            t.paragraphs,
            vec![
                labelled(0, 7_000, "Spoken. Mumble. Echo.", 1),
                labelled(9_800, 12_000, "Breath. Reply.", 2),
            ]
        );

        // An unlabelled first segment starts a paragraph that takes the
        // speaker of its first labelled segment.
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 1_000, "Um."), seg(5_000, 7_000, "Hello.")],
        );
        apply(&mut t, &[turn(5_000, 7_000, 3)]);
        assert_eq!(speakers_of(&t.segments), [None, Some(1)]);
        assert_eq!(t.paragraphs, vec![labelled(0, 7_000, "Um. Hello.", 1)]);
    }

    #[test]
    fn chunk_grain_uses_sentence_pieces_and_marks_approximate() {
        // One 20 s chunk without timestamps: 72 chars, the second sentence
        // starting at char 38.
        let text = "Thanks for coming in today, it helps. Sure, happy to help you out today.";
        let mut t = transcript(TimingGrain::Chunk, vec![seg(0, 20_000, text)]);
        let stats = apply(&mut t, &[turn(0, 10_400, 1), turn(10_600, 20_000, 3)]);
        assert_eq!(stats.speakers, 2);
        assert_eq!(
            t.segments,
            vec![
                labelled(0, 10_555, "Thanks for coming in today, it helps.", 1),
                labelled(10_555, 20_000, "Sure, happy to help you out today.", 2),
            ]
        );
        assert!(t.speakers.expect("speakers").approximate);
    }

    #[test]
    fn no_space_language_joins_without_spaces() {
        // 16 chars; sentences start at chars 0, 3 and 10.
        let mut t = transcript(
            TimingGrain::Chunk,
            vec![seg(0, 12_000, "你好。今天天气很好。是的，很好。")],
        );
        t.language = Some("zh".to_string());
        apply(&mut t, &[turn(0, 8_000, 1), turn(8_000, 12_000, 2)]);
        assert_eq!(
            t.segments,
            vec![
                labelled(0, 7_500, "你好。今天天气很好。", 1),
                labelled(7_500, 12_000, "是的，很好。", 2),
            ]
        );

        // Two segments of one speaker join without a space.
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 2_000, "你好。"), seg(2_000, 4_000, "很好。")],
        );
        t.language = Some("zh".to_string());
        apply(&mut t, &[turn(0, 4_000, 1)]);
        assert_eq!(t.paragraphs, vec![labelled(0, 4_000, "你好。很好。", 1)]);
        assert_eq!(t.text, "Speaker 1: 你好。很好。");
    }

    #[test]
    fn sentence_spans_rules() {
        let spans = |text: &str, no_space: bool| -> Vec<String> {
            sentence_spans(text, no_space)
                .into_iter()
                .map(|r| text[r].to_string())
                .collect()
        };
        assert_eq!(
            spans("  One. Two?! \"Three.\" Pi is 3.14 ok… End", false),
            ["One.", "Two?!", "\"Three.\"", "Pi is 3.14 ok…", "End"]
        );
        assert_eq!(spans("一。二！三", true), ["一。", "二！", "三"]);
        assert_eq!(spans("一。二", false), ["一。二"]);
        assert!(spans("   ", false).is_empty());
    }

    #[test]
    fn paragraphs_break_on_speaker_and_after_60s_at_sentence_end() {
        // One speaker for 150 s in 10 s segments; every other one ends a
        // sentence. A paragraph ends at the first sentence end at or after
        // 60 s.
        let segments = (0..15u64)
            .map(|i| {
                let text = if i % 2 == 0 { "and then" } else { "it ended." };
                seg(i * 10_000, (i + 1) * 10_000, text)
            })
            .collect();
        let mut t = transcript(TimingGrain::Segment, segments);
        apply(&mut t, &[turn(0, 150_000, 1)]);
        let starts: Vec<u64> = t.paragraphs.iter().map(|p| p.start_ms).collect();
        assert_eq!(starts, [0, 60_000, 120_000]);
        assert!(t.paragraphs.iter().all(|p| p.speaker == Some(1)));

        // Without any sentence end, it breaks at twice the limit.
        let segments = (0..15u64)
            .map(|i| seg(i * 10_000, (i + 1) * 10_000, "and so on"))
            .collect();
        let mut t = transcript(TimingGrain::Segment, segments);
        apply(&mut t, &[turn(0, 150_000, 1)]);
        let starts: Vec<u64> = t.paragraphs.iter().map(|p| p.start_ms).collect();
        assert_eq!(starts, [0, 120_000]);
    }

    #[test]
    fn unchanged_segments_keep_exact_text_and_times() {
        // Long (split into sentences) but all one speaker: the original
        // text, odd spacing included, and times come back exactly.
        let original = seg(1_234, 15_678, "First  sentence.   Second one!\tThird?");
        let mut t = transcript(TimingGrain::Segment, vec![original.clone()]);
        apply(&mut t, &[turn(0, 20_000, 2)]);
        assert_eq!(
            t.segments,
            vec![TranscriptSegment {
                speaker: Some(1),
                ..original
            }]
        );
    }

    #[test]
    fn no_turns_leaves_transcript_untouched() {
        let original = transcript(TimingGrain::Segment, vec![seg(0, 2_000, "Hello.")]);

        let mut t = original.clone();
        assert_eq!(apply(&mut t, &[]), MergeStats::default());
        assert_eq!(t, original);

        // Turns, but none near any text.
        let mut t = original.clone();
        let stats = apply(&mut t, &[turn(50_000, 60_000, 1)]);
        assert_eq!(stats.speakers, 0);
        assert_eq!(t, original);

        // Only invalid turns.
        let mut t = original.clone();
        assert_eq!(apply(&mut t, &[turn(500, 500, 1)]).speakers, 0);
        assert_eq!(t, original);
    }

    #[test]
    fn transcript_without_segments_is_timed_by_paragraphs() {
        let mut t = transcript(TimingGrain::Segment, vec![seg(0, 2_000, "Hello.")]);
        t.segments.clear();
        assert_eq!(apply(&mut t, &[turn(0, 2_000, 1)]).speakers, 1);
        assert_eq!(t.segments, vec![labelled(0, 2_000, "Hello.", 1)]);
    }

    #[test]
    fn render_text_labels() {
        let speakers = TranscriptSpeakers {
            speakers: vec![
                TranscriptSpeaker {
                    id: 1,
                    name: Some("Doug".to_string()),
                },
                TranscriptSpeaker { id: 2, name: None },
            ],
            approximate: false,
            unassigned_segments: 0,
            model: "m".to_string(),
        };
        let paragraphs = vec![
            labelled(0, 1_000, "Hi.", 1),
            labelled(1_000, 2_000, "Hello.", 2),
            seg(2_000, 3_000, "(music)"),
        ];
        assert_eq!(
            render_text(&paragraphs, Some(&speakers)),
            "Doug: Hi.\n\nSpeaker 2: Hello.\n\n(music)"
        );
        assert_eq!(render_text(&paragraphs, None), "Hi.\n\nHello.\n\n(music)");
        assert_eq!(speaker_label(&speakers, 1), "Doug");
        assert_eq!(speaker_label(&speakers, 2), "Speaker 2");
        assert_eq!(speaker_label(&speakers, 9), "Speaker 9");
    }

    #[test]
    fn rename_and_reset_name() {
        let mut t = transcript(
            TimingGrain::Segment,
            vec![seg(0, 2_000, "Hi."), seg(2_000, 4_000, "Hello.")],
        );
        apply(&mut t, &[turn(0, 2_000, 1), turn(2_000, 4_000, 2)]);

        rename_speaker(&mut t, 2, Some("  Dr. Lee: PT  ")).expect("rename");
        assert_eq!(t.text, "Speaker 1: Hi.\n\nDr. Lee: PT: Hello.");
        let names: Vec<Option<String>> = t
            .speakers
            .as_ref()
            .map(|s| s.speakers.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        assert_eq!(names, [None, Some("Dr. Lee: PT".to_string())]);

        rename_speaker(&mut t, 2, Some("")).expect("reset");
        assert_eq!(t.text, "Speaker 1: Hi.\n\nSpeaker 2: Hello.");
        rename_speaker(&mut t, 1, None).expect("reset an unnamed speaker");

        assert!(rename_speaker(&mut t, 3, Some("Nobody")).is_err());
        assert!(rename_speaker(&mut t, 1, Some("Line\nbreak")).is_err());
        let mut plain = transcript(TimingGrain::Segment, vec![seg(0, 1_000, "Hi.")]);
        assert!(rename_speaker(&mut plain, 1, Some("Doug")).is_err());
    }

    #[test]
    fn validate_speaker_name_rules() {
        assert_eq!(validate_speaker_name(None), Ok(None));
        assert_eq!(validate_speaker_name(Some("   ")), Ok(None));
        assert_eq!(
            validate_speaker_name(Some(" Doug ")),
            Ok(Some("Doug".to_string()))
        );
        assert_eq!(
            validate_speaker_name(Some("Dr. Lee: PT")),
            Ok(Some("Dr. Lee: PT".to_string()))
        );
        let sixty = "é".repeat(MAX_SPEAKER_NAME_CHARS);
        assert_eq!(validate_speaker_name(Some(&sixty)), Ok(Some(sixty.clone())));
        assert!(validate_speaker_name(Some(&format!("{sixty}x"))).is_err());
        assert!(validate_speaker_name(Some("a\nb")).is_err());
        assert!(validate_speaker_name(Some("a\tb")).is_err());
        assert!(validate_speaker_name(Some("a\u{7}b")).is_err());
    }

    #[test]
    fn cut_text_rules() {
        let cut = |text: &str, fraction: f64, no_space: bool| {
            cut_text(text, 0..text.len(), fraction, no_space)
                .map(|gap| (text[..gap.start].to_string(), text[gap.end..].to_string()))
        };
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        assert_eq!(cut("aa bb  cc", 0.6, false), pair("aa bb", "cc"));
        // Never an empty side.
        assert_eq!(cut("aa bb", 0.0, false), pair("aa", "bb"));
        assert_eq!(cut("aa bb", 1.0, false), pair("aa", "bb"));
        assert_eq!(cut("single", 0.5, false), None);
        assert_eq!(cut("你好，世界很大", 0.5, true), pair("你好，", "世界很大"));
        assert_eq!(cut("你好世界", 0.5, true), pair("你好", "世界"));
        // Never between a letter and its combining mark.
        assert_eq!(cut("e\u{301}e", 0.5, true), pair("e\u{301}", "e"));
    }

    /// Sortformer's turns on dialog2.wav (measured in the design) against
    /// segments at the true utterance times: eight alternating paragraphs.
    #[test]
    fn realistic_fixture() {
        const SPK1: &[(f64, f64)] = &[
            (0.00, 1.76),
            (1.92, 7.76),
            (18.32, 25.04),
            (34.08, 36.08),
            (36.16, 41.28),
            (51.28, 52.48),
            (52.56, 58.08),
        ];
        const SPK4: &[(f64, f64)] = &[
            (8.56, 9.04),
            (9.20, 10.56),
            (10.72, 17.60),
            (25.84, 29.12),
            (29.28, 33.36),
            (42.00, 50.48),
            (58.80, 60.88),
            (60.96, 65.12),
        ];
        const TRUTH: &[(f64, f64)] = &[
            (0.0, 7.95),
            (8.55, 17.77),
            (18.37, 25.31),
            (25.91, 33.51),
            (34.11, 41.46),
            (42.06, 50.66),
            (51.26, 58.28),
            (58.88, 65.28),
        ];
        let ms = |s: f64| (s * 1000.0).round() as u64;
        // Sortformer emits rows grouped by speaker, not by time.
        let turns: Vec<SpeakerTurn> = SPK1
            .iter()
            .map(|&(a, b)| turn(ms(a), ms(b), 1))
            .chain(SPK4.iter().map(|&(a, b)| turn(ms(a), ms(b), 4)))
            .collect();
        // Two sentences per utterance, as a timestamped model would give.
        let mut segments = Vec::new();
        for (i, &(a, b)) in TRUTH.iter().enumerate() {
            let mid = (a + b) / 2.0;
            segments.push(seg(ms(a), ms(mid), &format!("Turn {i} begins.")));
            segments.push(seg(ms(mid), ms(b), &format!("Turn {i} ends.")));
        }
        let mut t = transcript(TimingGrain::Segment, segments);
        let stats = apply(&mut t, &turns);
        assert_eq!(stats.speakers, 2);
        assert_eq!(stats.unassigned, 0);
        assert_eq!(t.paragraphs.len(), 8);
        for (i, paragraph) in t.paragraphs.iter().enumerate() {
            assert_eq!(paragraph.speaker, Some(if i % 2 == 0 { 1 } else { 2 }));
            assert_eq!(paragraph.text, format!("Turn {i} begins. Turn {i} ends."));
            assert_eq!(paragraph.start_ms, ms(TRUTH[i].0));
        }
    }

    #[test]
    fn options_follow_the_transcript_language() {
        let mut t = transcript(TimingGrain::Segment, Vec::new());
        assert!(!MergeOptions::for_transcript(&t).no_space);
        t.language = Some("ja".to_string());
        assert!(MergeOptions::for_transcript(&t).no_space);
    }
}
