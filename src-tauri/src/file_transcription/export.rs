//! Transcript rendering (TXT, Markdown, SRT, WebVTT, JSON), the subtitle cue
//! builder and atomic file writes. The single source of truth for export
//! formats: the GUI export command and the headless CLI both render here.
//!
//! Every renderer emits UTF-8 without a BOM and `\n` line endings; a
//! non-empty output ends with exactly one `\n`.

use crate::file_transcription::types::{
    is_no_space_language, FileTranscript, TranscriptFormat, TranscriptSegment, TranscriptStatus,
};
use chrono::DateTime;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Longest subtitle line, in `char`s.
const MAX_LINE_CHARS: usize = 42;
/// Longest subtitle line for text written without spaces between words.
const MAX_LINE_CHARS_NO_SPACE: usize = 16;
const MAX_LINES_PER_CUE: usize = 2;
/// Longest time a single cue stays on screen.
const MAX_CUE_MS: u64 = 7_000;
/// Length given to a segment whose end is not after its start.
const FALLBACK_SEGMENT_MS: u64 = 1_000;

pub fn render(t: &FileTranscript, format: TranscriptFormat) -> String {
    match format {
        TranscriptFormat::Txt => render_txt(t),
        TranscriptFormat::Md => render_markdown(t),
        TranscriptFormat::Srt => render_srt(t),
        TranscriptFormat::Vtt => render_vtt(t),
        TranscriptFormat::Json => render_json(t),
    }
}

/// The transcript text: paragraphs separated by a blank line, no header and
/// no hard wrapping. Empty when nothing was said.
pub fn render_txt(t: &FileTranscript) -> String {
    let text = t.text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("{text}\n")
    }
}

/// A short metadata header, then one block per paragraph prefixed with its
/// start time. Labels are English and the date is UTC, so the output does
/// not depend on the machine it was rendered on.
pub fn render_markdown(t: &FileTranscript) -> String {
    let title = markdown_text(&t.source_name, " ");
    let title = if title.is_empty() {
        "Transcript"
    } else {
        &title
    };
    let mut out = format!("# {title}\n\n");
    out.push_str(&format!(
        "- **Duration:** {}\n",
        format_clock(t.duration_ms)
    ));
    let model = t
        .model_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&t.model_id);
    if !model.trim().is_empty() {
        out.push_str(&format!("- **Model:** {}\n", markdown_text(model, " ")));
    }
    if let Some(language) = t.language.as_deref().filter(|l| !l.trim().is_empty()) {
        out.push_str(&format!(
            "- **Language:** {}\n",
            markdown_text(language, " ")
        ));
    }
    if let Some(created) = DateTime::from_timestamp(t.created_at, 0) {
        out.push_str(&format!(
            "- **Transcribed:** {}\n",
            created.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    if t.status == TranscriptStatus::Partial {
        match t.status_detail.as_deref().map(str::trim) {
            Some(detail) if !detail.is_empty() => out.push_str(&format!(
                "- **Status:** Incomplete — {}\n",
                markdown_text(detail, " ")
            )),
            _ => out.push_str("- **Status:** Incomplete\n"),
        }
    }
    out.push_str("\n---\n");

    if t.paragraphs.is_empty() {
        // Only reachable for a transcript stored without paragraph timing:
        // keep its text rather than export an empty document.
        for paragraph in t.text.split("\n\n") {
            let body = markdown_text(paragraph, "\n");
            if !body.is_empty() {
                out.push_str(&format!("\n{body}\n"));
            }
        }
    }
    for paragraph in &t.paragraphs {
        let body = markdown_text(&paragraph.text, "\n");
        if !body.is_empty() {
            out.push_str(&format!(
                "\n**[{}]** {}\n",
                format_clock(paragraph.start_ms),
                body
            ));
        }
    }
    out
}

/// SubRip subtitles: numbered cues separated by a blank line, no escaping.
/// Empty when there is nothing to show.
pub fn render_srt(t: &FileTranscript) -> String {
    build_cues(timed_text(t), t.language.as_deref())
        .iter()
        .enumerate()
        .map(|(i, cue)| {
            format!(
                "{}\n{} --> {}\n{}\n",
                i + 1,
                format_srt_timestamp(cue.start_ms),
                format_srt_timestamp(cue.end_ms),
                cue.lines.join("\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// WebVTT subtitles: the `WEBVTT` header, then unnumbered cues separated by a
/// blank line. `&`, `<` and `>` are escaped, which also defuses a `-->` in
/// the text.
pub fn render_vtt(t: &FileTranscript) -> String {
    let cues = build_cues(timed_text(t), t.language.as_deref());
    if cues.is_empty() {
        return "WEBVTT\n".to_string();
    }
    let body = cues
        .iter()
        .map(|cue| {
            let lines: Vec<String> = cue.lines.iter().map(|line| escape_vtt(line)).collect();
            format!(
                "{} --> {}\n{}\n",
                format_vtt_timestamp(cue.start_ms),
                format_vtt_timestamp(cue.end_ms),
                lines.join("\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("WEBVTT\n\n{body}")
}

/// The whole `FileTranscript` as pretty-printed JSON (the shape of the
/// TypeScript binding).
pub fn render_json(t: &FileTranscript) -> String {
    // FileTranscript holds only strings, integers and unit enums, which
    // serde_json always serializes.
    let json = serde_json::to_string_pretty(t).expect("FileTranscript serializes to JSON");
    json + "\n"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub lines: Vec<String>,
}

/// Turns timed segments into subtitle cues of at most two lines (42 chars
/// each, 16 for text written without spaces) shown for at most 7 s. A
/// segment's time is shared among its cues in proportion to their text, and
/// the result is ordered and free of overlaps: every cue ends after it
/// starts and no later than the next cue starts.
pub fn build_cues(segments: &[TranscriptSegment], language: Option<&str>) -> Vec<Cue> {
    let mut cues: Vec<Cue> = Vec::new();
    for segment in segments {
        let text = segment
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        let start = segment.start_ms;
        let end = if segment.end_ms > start {
            segment.end_ms
        } else {
            start.saturating_add(FALLBACK_SEGMENT_MS)
        };
        let no_space =
            is_no_space_language(language) || (!text.contains(' ') && text.chars().any(is_cjk));
        let limit = if no_space {
            MAX_LINE_CHARS_NO_SPACE
        } else {
            MAX_LINE_CHARS
        };
        let units = split_units(&text, no_space, limit);

        let mut segment_cues = Vec::new();
        layout_cues(&units, start, end, limit, &mut segment_cues);
        for cue in segment_cues {
            push_in_order(&mut cues, cue);
        }
    }
    cues
}

/// "HH:MM:SS", rounded down; hours grow past two digits ("100:00:00").
pub fn format_clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// "HH:MM:SS,mmm"
pub fn format_srt_timestamp(ms: u64) -> String {
    format!("{},{:03}", format_clock(ms), ms % 1000)
}

/// "HH:MM:SS.mmm"
pub fn format_vtt_timestamp(ms: u64) -> String {
    format!("{}.{:03}", format_clock(ms), ms % 1000)
}

/// The transcript on one line: paragraph breaks ("\n\n") become a space, or
/// nothing for languages written without spaces.
pub fn single_line(text: &str, language: Option<&str>) -> String {
    let separator = if is_no_space_language(language) {
        ""
    } else {
        " "
    };
    text.replace("\n\n", separator)
}

/// The suggested export file name: the source name with its extension
/// replaced ("interview.final.m4a" → "interview.final.srt"). Characters that
/// some platform forbids in file names become `_`; an empty stem becomes
/// "transcript".
pub fn default_export_file_name(source_name: &str, format: TranscriptFormat) -> String {
    let stem = match source_name.rfind('.') {
        Some(dot) if dot > 0 && !source_name[dot..].contains(['/', '\\']) => &source_name[..dot],
        _ => source_name,
    };
    let sanitized: String = stem
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let sanitized = sanitized.trim();
    let stem = if sanitized.chars().all(|c| c == '.') {
        "transcript"
    } else {
        sanitized
    };
    format!("{stem}.{}", format.extension())
}

/// Symbolic links followed before giving up, as the OS does.
const MAX_SYMLINK_HOPS: usize = 40;

/// Replaces `path` with `contents` so readers never see a half-written file:
/// writes `<name>.tmp-<pid>` next to it, syncs it, then renames it over the
/// target. The temporary file is removed again if any step fails.
///
/// A symbolic link is followed and its target replaced, keeping the target's
/// permissions (on Unix). A target that exists but is neither a regular file
/// nor a folder (a pipe, a terminal, `/dev/stdout`) cannot be replaced, so
/// it is written to directly.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if is_special_file(path) {
        let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
        file.write_all(contents.as_bytes())?;
        return file.flush();
    }
    let path = resolve_symlinks(path)?;
    let temp_path = temp_path_for(&path)?;

    let result = write_and_replace(&temp_path, &path, contents.as_bytes());
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Fails, without touching `path`, when `write_atomic(path, ..)` surely
/// would because its folder is missing or cannot take the temporary file.
/// A special file (see `write_atomic`) is not opened here: opening a pipe
/// can block, or end it for its reader.
pub fn check_writable(path: &Path) -> io::Result<()> {
    if is_special_file(path) {
        return Ok(());
    }
    let temp_path = temp_path_for(&resolve_symlinks(path)?)?;
    File::create(&temp_path)?;
    fs::remove_file(&temp_path)
}

/// True when `path` (after links) exists and is neither a regular file nor
/// a folder.
fn is_special_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| !metadata.is_file() && !metadata.is_dir())
}

/// `path` with the links in its last component followed, whether or not
/// the final target exists yet.
fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&path)?;
                // A relative target is relative to the link's folder; an
                // absolute one replaces the whole path.
                path = match path.parent() {
                    Some(folder) => folder.join(target),
                    None => target,
                };
            }
            _ => return Ok(path),
        }
    }
    Err(io::Error::other(format!(
        "too many levels of symbolic links: {}",
        path.display()
    )))
}

/// `<name>.tmp-<pid>` next to `path`.
fn temp_path_for(path: &Path) -> io::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a file path: {}", path.display()),
        )
    })?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(format!(".tmp-{}", std::process::id()));
    Ok(path.with_file_name(temp_name))
}

fn write_and_replace(temp_path: &Path, path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = File::create(temp_path)?;
    // Keep the mode of the file being replaced, set before any text is
    // written so a private transcript is never readable by others. (Windows
    // has only a read-only flag, which would block the replace.)
    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(path) {
        if metadata.is_file() {
            file.set_permissions(metadata.permissions())?;
        }
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    match fs::rename(temp_path, path) {
        Ok(()) => Ok(()),
        // Windows refuses to replace some files in place (read-only
        // attribute, open handles); retry once the old file is gone.
        #[cfg(windows)]
        Err(_) if path.is_file() => {
            fs::remove_file(path)?;
            fs::rename(temp_path, path)
        }
        Err(e) => Err(e),
    }
}

/// Subtitles need timed text: the segments, or the paragraphs of a
/// transcript stored without segments.
fn timed_text(t: &FileTranscript) -> &[TranscriptSegment] {
    if t.segments.is_empty() {
        &t.paragraphs
    } else {
        &t.segments
    }
}

fn escape_vtt(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes text for Markdown. Each line is trimmed and escaped, blank lines
/// are dropped (so a paragraph stays one block) and the lines are joined
/// with `separator`.
fn markdown_text(text: &str, separator: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(escape_markdown_line)
        .collect::<Vec<_>>()
        .join(separator)
}

/// Backslash-escapes inline markup characters, and a heading, list or
/// ordered-list marker at the start of the line, so the line reads literally.
fn escape_markdown_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 4);
    if line.starts_with(['#', '-', '+']) {
        out.push('\\');
    }
    // "12." or "12)" would start an ordered list. A backslash only escapes
    // punctuation, so it goes before the delimiter ("12\.").
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let list_delimiter =
        (digits > 0 && matches!(line.as_bytes().get(digits), Some(b'.' | b')'))).then_some(digits);
    for (i, c) in line.char_indices() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|')
            || list_delimiter == Some(i)
        {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// One unbreakable piece of a subtitle line: a word, a CJK character, or a
/// part of a word too long for one line.
#[derive(Debug, Clone)]
struct Unit {
    text: String,
    chars: usize,
    /// Separated from the previous unit by a space (dropped at a line start).
    space_before: bool,
}

/// Splits whitespace-collapsed text into units. In no-space mode each CJK
/// character is a unit and the runs between them (Latin words, digits, Thai
/// or Khmer phrases) are words. Words longer than `limit` are hard-split.
fn split_units(text: &str, no_space: bool, limit: usize) -> Vec<Unit> {
    let mut units = Vec::new();
    for (index, word) in text.split(' ').enumerate() {
        let mut space_before = index > 0;
        let clusters = clusters(word);
        if !no_space {
            push_word(&mut units, &clusters, space_before, limit);
            continue;
        }
        let mut run_start = 0;
        for (i, cluster) in clusters.iter().enumerate() {
            if !cluster.starts_with(is_cjk) {
                continue;
            }
            if run_start < i {
                push_word(&mut units, &clusters[run_start..i], space_before, limit);
                space_before = false;
            }
            units.push(Unit {
                text: (*cluster).to_string(),
                chars: cluster.chars().count(),
                space_before,
            });
            space_before = false;
            run_start = i + 1;
        }
        if run_start < clusters.len() {
            push_word(&mut units, &clusters[run_start..], space_before, limit);
        }
    }
    units
}

/// Adds one word as a unit, or as pieces of at most `limit` chars (never
/// split inside a cluster) when it does not fit on a line.
fn push_word(units: &mut Vec<Unit>, clusters: &[&str], space_before: bool, limit: usize) {
    let mut piece = String::new();
    let mut chars = 0;
    let mut space_before = space_before;
    for cluster in clusters {
        let len = cluster.chars().count();
        if chars > 0 && chars + len > limit {
            units.push(Unit {
                text: std::mem::take(&mut piece),
                chars,
                space_before,
            });
            chars = 0;
            space_before = false;
        }
        piece.push_str(cluster);
        chars += len;
    }
    if chars > 0 {
        units.push(Unit {
            text: piece,
            chars,
            space_before,
        });
    }
}

/// Splits text into user-perceived characters, approximately: a character
/// plus the combining marks, joiners and modifiers that follow it, so a line
/// break never separates a Thai vowel or tone mark from its consonant.
fn clusters(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        let attaches = prev.is_some_and(|p| {
            !c.is_whitespace()
                && (is_combining(c) || matches!(p, '\u{200D}' | '\u{1039}' | '\u{17D2}'))
        });
        if !attaches && i > start {
            out.push(&text[start..i]);
            start = i;
        }
        prev = Some(c);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Characters that belong to the one before them: combining marks; Thai,
/// Lao, Myanmar and Khmer vowel signs and tone marks; kana voicing marks;
/// joiners, variation selectors and emoji skin tones. (The zero-width
/// joiner, Myanmar virama and Khmer coeng also bind the character after
/// them; `clusters` handles that.)
fn is_combining(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0300..=0x036F
            | 0x0E31
            | 0x0E34..=0x0E3A
            | 0x0E47..=0x0E4E
            | 0x0EB1
            | 0x0EB4..=0x0EBC
            | 0x0EC8..=0x0ECD
            | 0x102B..=0x103E
            | 0x1056..=0x1059
            | 0x105E..=0x1060
            | 0x1062..=0x1064
            | 0x1067..=0x106D
            | 0x1071..=0x1074
            | 0x1082..=0x108D
            | 0x108F
            | 0x109A..=0x109D
            | 0x17B4..=0x17D3
            | 0x17DD
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200C..=0x200D
            | 0x20D0..=0x20FF
            | 0x3099..=0x309A
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0x1F3FB..=0x1F3FF
            | 0xE0100..=0xE01EF
    )
}

/// Han, kana, Hangul, bopomofo, CJK punctuation and full-width forms: scripts
/// whose subtitle lines may break between any two characters.
fn is_cjk(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF
            | 0x2E80..=0x2FFF
            | 0x3000..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFFEF
            | 0x20000..=0x3FFFF
    )
}

/// Greedy line wrap. Returns the unit range of each line.
fn wrap_lines(units: &[Unit], limit: usize) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut width = 0;
    for (i, unit) in units.iter().enumerate() {
        if i == line_start {
            width = unit.chars;
            continue;
        }
        let joined = width + usize::from(unit.space_before) + unit.chars;
        if joined > limit {
            lines.push(line_start..i);
            line_start = i;
            width = unit.chars;
        } else {
            width = joined;
        }
    }
    if line_start < units.len() {
        lines.push(line_start..units.len());
    }
    lines
}

fn line_text(units: &[Unit]) -> String {
    let mut line = String::new();
    for (i, unit) in units.iter().enumerate() {
        if i > 0 && unit.space_before {
            line.push(' ');
        }
        line.push_str(&unit.text);
    }
    line
}

fn weight(units: &[Unit]) -> usize {
    units.iter().map(|unit| unit.chars).sum()
}

/// Lays out `units`, shown from `start` to `end`, as cues: consecutive pairs
/// of lines become cues timed in proportion to their text, and a cue longer
/// than MAX_CUE_MS is split at the unit boundary nearest its middle.
fn layout_cues(units: &[Unit], start: u64, end: u64, limit: usize, out: &mut Vec<Cue>) {
    if units.is_empty() {
        return;
    }
    let lines = wrap_lines(units, limit);

    if lines.len() > MAX_LINES_PER_CUE {
        let groups: Vec<Range<usize>> = lines
            .chunks(MAX_LINES_PER_CUE)
            .map(|pair| pair[0].start..pair[pair.len() - 1].end)
            .collect();
        let weights: Vec<usize> = groups.iter().map(|g| weight(&units[g.clone()])).collect();
        for (group, (s, e)) in groups.iter().zip(proportional_spans(&weights, start, end)) {
            layout_cues(&units[group.clone()], s, e, limit, out);
        }
        return;
    }

    let mut end = end;
    if end - start > MAX_CUE_MS {
        if units.len() >= 2 {
            let (left, right) = units.split_at(midpoint_split(units));
            let spans = proportional_spans(&[weight(left), weight(right)], start, end);
            layout_cues(left, spans[0].0, spans[0].1, limit, out);
            layout_cues(right, spans[1].0, spans[1].1, limit, out);
            return;
        }
        end = start + MAX_CUE_MS;
    }

    out.push(Cue {
        start_ms: start,
        end_ms: end.max(start.saturating_add(1)),
        lines: lines
            .iter()
            .map(|line| line_text(&units[line.clone()]))
            .collect(),
    });
}

/// The unit boundary (1..len) closest to the middle of the text. Needs at
/// least two units.
fn midpoint_split(units: &[Unit]) -> usize {
    let total = weight(units);
    let mut before = 0;
    let mut best = (usize::MAX, 1);
    for (i, unit) in units.iter().enumerate().take(units.len() - 1) {
        before += unit.chars;
        let distance = (2 * before).abs_diff(total);
        if distance < best.0 {
            best = (distance, i + 1);
        }
    }
    best.1
}

/// Splits `start..end` into consecutive spans proportional to `weights`
/// (rounded down); the last span ends exactly at `end`.
fn proportional_spans(weights: &[usize], start: u64, end: u64) -> Vec<(u64, u64)> {
    let total = weights.iter().map(|&w| w as u128).sum::<u128>().max(1);
    let duration = u128::from(end.saturating_sub(start));
    let mut before = 0u128;
    weights
        .iter()
        .map(|&w| {
            let span_start = start + (duration * before / total) as u64;
            before += w as u128;
            let span_end = start + (duration * before / total) as u64;
            (span_start, span_end)
        })
        .collect()
}

/// Appends a cue after resolving an overlap with the previous one: a cue
/// starting inside the previous one cuts it short; a cue starting no later
/// than the previous one is moved after it.
fn push_in_order(cues: &mut Vec<Cue>, mut cue: Cue) {
    if let Some(prev) = cues.last_mut() {
        if cue.start_ms < prev.end_ms {
            if cue.start_ms > prev.start_ms {
                prev.end_ms = cue.start_ms;
            } else {
                cue.start_ms = prev.end_ms;
                cue.end_ms = cue.end_ms.max(cue.start_ms.saturating_add(1));
            }
        }
    }
    cues.push(cue);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::types::TimingGrain;

    fn seg(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment::new(start_ms, end_ms, text)
    }

    fn cue(start_ms: u64, end_ms: u64, lines: &[&str]) -> Cue {
        Cue {
            start_ms,
            end_ms,
            lines: lines.iter().map(|l| l.to_string()).collect(),
        }
    }

    /// Each renderer reads either the paragraphs (TXT, Markdown) or the
    /// segments (SRT, VTT), so the two lists are independent here.
    fn sample() -> FileTranscript {
        FileTranscript {
            source_path: "/recordings/interview.m4a".to_string(),
            source_name: "interview.m4a".to_string(),
            duration_ms: 3_725_000,
            model_id: "small".to_string(),
            model_name: Some("Whisper Small".to_string()),
            language: Some("en".to_string()),
            status: TranscriptStatus::Partial,
            status_detail: Some("Decoding stopped early at 00:41:10".to_string()),
            timing: TimingGrain::Segment,
            text: "Hello and welcome to the show. Today we talk about transcription.\n\n\
                   Second paragraph text."
                .to_string(),
            paragraphs: vec![
                seg(
                    0,
                    27_000,
                    "Hello and welcome to the show. Today we talk about transcription.",
                ),
                seg(27_000, 31_500, "Second paragraph text."),
            ],
            segments: vec![
                seg(0, 3_200, "Hello and welcome to the show."),
                seg(
                    3_200,
                    7_950,
                    "Today we talk about transcription and why chunking matters.",
                ),
            ],
            created_at: 1_700_000_000,
            history_id: None,
            speakers: None,
        }
    }

    fn empty() -> FileTranscript {
        FileTranscript {
            status: TranscriptStatus::Completed,
            status_detail: None,
            text: String::new(),
            paragraphs: Vec::new(),
            segments: Vec::new(),
            ..sample()
        }
    }

    /// Cues must be ordered, non-empty, non-overlapping and at most 7 s long.
    fn assert_well_formed(cues: &[Cue]) {
        for cue in cues {
            assert!(cue.end_ms > cue.start_ms, "empty cue {cue:?}");
            assert!(cue.end_ms - cue.start_ms <= MAX_CUE_MS, "long cue {cue:?}");
            assert!(!cue.lines.is_empty() && cue.lines.len() <= MAX_LINES_PER_CUE);
            assert!(cue.lines.iter().all(|line| !line.is_empty()));
        }
        for pair in cues.windows(2) {
            assert!(pair[0].end_ms <= pair[1].start_ms, "overlap {pair:?}");
        }
    }

    #[test]
    fn txt_golden() {
        assert_eq!(
            render(&sample(), TranscriptFormat::Txt),
            "Hello and welcome to the show. Today we talk about transcription.\n\n\
             Second paragraph text.\n"
        );
        assert_eq!(render_txt(&empty()), "");
    }

    #[test]
    fn markdown_golden() {
        let expected = "# interview.m4a

- **Duration:** 01:02:05
- **Model:** Whisper Small
- **Language:** en
- **Transcribed:** 2023-11-14 22:13 UTC
- **Status:** Incomplete — Decoding stopped early at 00:41:10

---

**[00:00:00]** Hello and welcome to the show. Today we talk about transcription.

**[00:00:27]** Second paragraph text.
";
        assert_eq!(render(&sample(), TranscriptFormat::Md), expected);
    }

    #[test]
    fn markdown_omits_unknown_language_and_status_of_complete_transcripts() {
        let t = FileTranscript {
            model_name: None,
            language: None,
            status: TranscriptStatus::Completed,
            status_detail: None,
            ..sample()
        };
        let md = render_markdown(&t);
        assert!(md.starts_with(
            "# interview.m4a\n\n\
             - **Duration:** 01:02:05\n\
             - **Model:** small\n\
             - **Transcribed:** 2023-11-14 22:13 UTC\n\
             \n---\n\n"
        ));
        assert!(!md.contains("Language"));
        assert!(!md.contains("Status"));
    }

    #[test]
    fn markdown_of_empty_transcript_is_header_only() {
        assert_eq!(
            render_markdown(&empty()),
            "# interview.m4a\n\n\
             - **Duration:** 01:02:05\n\
             - **Model:** Whisper Small\n\
             - **Language:** en\n\
             - **Transcribed:** 2023-11-14 22:13 UTC\n\
             \n---\n"
        );
        let untitled = FileTranscript {
            source_name: String::new(),
            ..empty()
        };
        assert!(render_markdown(&untitled).starts_with("# Transcript\n\n"));
    }

    #[test]
    fn markdown_escapes_markup_and_block_markers() {
        assert_eq!(escape_markdown_line("# Not a heading"), "\\# Not a heading");
        assert_eq!(escape_markdown_line("- not a list"), "\\- not a list");
        assert_eq!(escape_markdown_line("+ not a list"), "\\+ not a list");
        assert_eq!(escape_markdown_line("1. Not a list"), "1\\. Not a list");
        assert_eq!(escape_markdown_line("12) Not a list"), "12\\) Not a list");
        assert_eq!(escape_markdown_line("2024 was a year"), "2024 was a year");
        assert_eq!(escape_markdown_line("3.5 percent"), "3\\.5 percent");
        assert_eq!(
            escape_markdown_line(r"a*b_c`d[e]f<g>h|i\j"),
            r"a\*b\_c\`d\[e\]f\<g\>h\|i\\j"
        );

        let t = FileTranscript {
            source_name: "my_file*.m4a".to_string(),
            paragraphs: vec![seg(61_000, 70_000, "# Heading?\n\n  - dash\n1. one")],
            ..empty()
        };
        let md = render_markdown(&t);
        assert!(md.starts_with("# my\\_file\\*.m4a\n"), "{md}");
        assert!(
            md.ends_with("\n---\n\n**[00:01:01]** \\# Heading?\n\\- dash\n1\\. one\n"),
            "{md}"
        );
    }

    #[test]
    fn srt_golden() {
        let expected = "1
00:00:00,000 --> 00:00:03,200
Hello and welcome to the show.

2
00:00:03,200 --> 00:00:07,950
Today we talk about transcription and why
chunking matters.
";
        assert_eq!(render(&sample(), TranscriptFormat::Srt), expected);
        assert_eq!(render_srt(&empty()), "");
    }

    #[test]
    fn vtt_golden() {
        let expected = "WEBVTT

00:00:00.000 --> 00:00:03.200
Hello and welcome to the show.

00:00:03.200 --> 00:00:07.950
Today we talk about transcription and why
chunking matters.
";
        assert_eq!(render(&sample(), TranscriptFormat::Vtt), expected);
        assert_eq!(render_vtt(&empty()), "WEBVTT\n");
    }

    #[test]
    fn vtt_escapes_markup_and_arrows_but_srt_does_not() {
        let t = FileTranscript {
            segments: vec![seg(0, 2_000, "Tom & Jerry <i>live</i> --> now")],
            ..sample()
        };
        assert_eq!(
            render_vtt(&t),
            "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\n\
             Tom &amp; Jerry &lt;i&gt;live&lt;/i&gt; --&gt; now\n"
        );
        assert_eq!(
            render_srt(&t),
            "1\n00:00:00,000 --> 00:00:02,000\nTom & Jerry <i>live</i> --> now\n"
        );
    }

    #[test]
    fn subtitles_fall_back_to_paragraphs_without_segments() {
        let t = FileTranscript {
            segments: Vec::new(),
            ..sample()
        };
        let srt = render_srt(&t);
        assert!(srt.starts_with("1\n00:00:00,000 --> "), "{srt}");
        assert!(srt.ends_with("00:00:27,000 --> 00:00:31,500\nSecond paragraph text.\n"));
    }

    #[test]
    fn json_golden_and_round_trip() {
        let expected = r#"{
  "source_path": "/recordings/interview.m4a",
  "source_name": "interview.m4a",
  "duration_ms": 3725000,
  "model_id": "small",
  "model_name": "Whisper Small",
  "language": "en",
  "status": "partial",
  "status_detail": "Decoding stopped early at 00:41:10",
  "timing": "segment",
  "text": "Hello and welcome to the show. Today we talk about transcription.\n\nSecond paragraph text.",
  "paragraphs": [
    {
      "start_ms": 0,
      "end_ms": 27000,
      "text": "Hello and welcome to the show. Today we talk about transcription."
    },
    {
      "start_ms": 27000,
      "end_ms": 31500,
      "text": "Second paragraph text."
    }
  ],
  "segments": [
    {
      "start_ms": 0,
      "end_ms": 3200,
      "text": "Hello and welcome to the show."
    },
    {
      "start_ms": 3200,
      "end_ms": 7950,
      "text": "Today we talk about transcription and why chunking matters."
    }
  ],
  "created_at": 1700000000,
  "history_id": null
}
"#;
        let json = render(&sample(), TranscriptFormat::Json);
        assert_eq!(json, expected);
        let parsed: FileTranscript = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, sample());
    }

    #[test]
    fn timestamps() {
        let cases = [
            (0, "00:00:00", "00:00:00,000", "00:00:00.000"),
            (999, "00:00:00", "00:00:00,999", "00:00:00.999"),
            (1_000, "00:00:01", "00:00:01,000", "00:00:01.000"),
            (59_999, "00:00:59", "00:00:59,999", "00:00:59.999"),
            (3_599_999, "00:59:59", "00:59:59,999", "00:59:59.999"),
            (3_600_000, "01:00:00", "01:00:00,000", "01:00:00.000"),
            (360_000_000, "100:00:00", "100:00:00,000", "100:00:00.000"),
        ];
        for (ms, clock, srt, vtt) in cases {
            assert_eq!(format_clock(ms), clock, "{ms}");
            assert_eq!(format_srt_timestamp(ms), srt, "{ms}");
            assert_eq!(format_vtt_timestamp(ms), vtt, "{ms}");
        }
    }

    #[test]
    fn cues_wrap_greedily_at_42_chars() {
        let cues = build_cues(
            &[seg(
                0,
                5_000,
                "The quick brown fox jumps over the lazy dog and keeps running far away",
            )],
            Some("en"),
        );
        assert_eq!(
            cues,
            vec![cue(
                0,
                5_000,
                &[
                    "The quick brown fox jumps over the lazy",
                    "dog and keeps running far away"
                ]
            )]
        );
    }

    #[test]
    fn cues_hold_at_most_two_lines_and_share_time_by_length() {
        let text = "word ".repeat(40);
        let cues = build_cues(&[seg(1_000, 7_000, &text)], None);
        assert_well_formed(&cues);
        assert!(cues.len() >= 3, "{cues:?}");
        for cue in &cues {
            assert!(cue.lines.iter().all(|line| line.chars().count() <= 42));
        }
        // All words survive, in order.
        let words: Vec<String> = cues.iter().flat_map(|c| c.lines.clone()).collect();
        assert_eq!(words.join(" "), text.trim());
        // Contiguous spans covering the segment exactly.
        assert_eq!(cues[0].start_ms, 1_000);
        assert_eq!(cues.last().unwrap().end_ms, 7_000);
        for pair in cues.windows(2) {
            assert_eq!(pair[0].end_ms, pair[1].start_ms);
        }
    }

    #[test]
    fn overlong_word_is_hard_split() {
        let word = "a".repeat(100);
        let cues = build_cues(&[seg(0, 3_000, &word)], None);
        let (full, rest) = ("a".repeat(42), "a".repeat(16));
        assert_eq!(
            cues,
            vec![
                cue(0, 2_520, &[full.as_str(), full.as_str()]),
                cue(2_520, 3_000, &[rest.as_str()]),
            ]
        );
    }

    #[test]
    fn cjk_wraps_by_character_at_16() {
        let text = "今日はとても良い天気ですね。明日も晴れるといいですね。";
        let expected = vec![cue(
            0,
            4_000,
            &["今日はとても良い天気ですね。明日", "も晴れるといいですね。"],
        )];
        assert_eq!(build_cues(&[seg(0, 4_000, text)], Some("ja")), expected);
        // Detected from the text when the language is unknown or wrong.
        assert_eq!(build_cues(&[seg(0, 4_000, text)], None), expected);
        assert_eq!(build_cues(&[seg(0, 4_000, text)], Some("en")), expected);
    }

    #[test]
    fn cjk_keeps_latin_words_whole() {
        let cues = build_cues(
            &[seg(0, 3_000, "これは新しいスマートフォンのiPhoneです")],
            Some("ja"),
        );
        assert_eq!(
            cues,
            vec![cue(
                0,
                3_000,
                &["これは新しいスマートフォンの", "iPhoneです"]
            )]
        );
    }

    #[test]
    fn line_breaks_never_split_combining_marks() {
        // 15 consonants, then a consonant carrying a vowel sign (2 chars):
        // the 16-char line ends before that pair.
        let text = format!("{}กี{}", "ก".repeat(15), "ข".repeat(3));
        let cues = build_cues(&[seg(0, 2_000, &text)], Some("th"));
        assert_eq!(
            cues,
            vec![cue(0, 2_000, &["ก".repeat(15).as_str(), "กีขขข"])]
        );
        assert_eq!(clusters("e\u{301}x"), vec!["e\u{301}", "x"]);
        assert_eq!(clusters("👩\u{200D}💻!"), vec!["👩\u{200D}💻", "!"]);
    }

    #[test]
    fn long_cues_are_split_at_the_text_midpoint() {
        let cues = build_cues(&[seg(0, 20_000, "one two three four")], None);
        assert_eq!(
            cues,
            vec![
                cue(0, 4_000, &["one"]),
                cue(4_000, 8_000, &["two"]),
                cue(8_000, 14_666, &["three"]),
                cue(14_666, 20_000, &["four"]),
            ]
        );
        // A single word can't be split; it is shown for 7 s at most.
        assert_eq!(
            build_cues(&[seg(5_000, 30_000, "hello")], None),
            vec![cue(5_000, 12_000, &["hello"])]
        );
    }

    #[test]
    fn segments_without_duration_get_one_second() {
        assert_eq!(
            build_cues(&[seg(5_000, 5_000, "hi")], None),
            vec![cue(5_000, 6_000, &["hi"])]
        );
        assert_eq!(
            build_cues(&[seg(5_000, 4_000, "hi")], None),
            vec![cue(5_000, 6_000, &["hi"])]
        );
    }

    #[test]
    fn blank_segments_are_skipped_and_whitespace_collapsed() {
        let cues = build_cues(
            &[
                seg(0, 1_000, "   "),
                seg(1_000, 2_000, ""),
                seg(2_000, 3_000, "  hello \n  world  "),
            ],
            None,
        );
        assert_eq!(cues, vec![cue(2_000, 3_000, &["hello world"])]);
        assert!(build_cues(&[], None).is_empty());
    }

    #[test]
    fn overlapping_segments_are_put_in_order() {
        // Starts inside the previous cue: the previous one is cut short.
        assert_eq!(
            build_cues(&[seg(0, 4_000, "first"), seg(3_000, 5_000, "second")], None),
            vec![cue(0, 3_000, &["first"]), cue(3_000, 5_000, &["second"])]
        );
        // Starts no later than the previous cue: moved after it.
        assert_eq!(
            build_cues(&[seg(0, 4_000, "first"), seg(0, 2_000, "second")], None),
            vec![cue(0, 4_000, &["first"]), cue(4_000, 4_001, &["second"])]
        );
    }

    #[test]
    fn cue_times_are_monotonic_and_non_overlapping() {
        let long = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(6);
        let segments = vec![
            seg(0, 1, &long),
            seg(0, 25_000, &long),
            seg(24_000, 24_500, "tiny"),
            seg(24_100, 60_000, &long),
            seg(59_000, 59_000, "zero length"),
            seg(
                10_000,
                90_000,
                "out of order and long enough to need a few lines of text",
            ),
        ];
        let cues = build_cues(&segments, Some("en"));
        assert_well_formed(&cues);
        assert!(cues.len() > 10);
    }

    #[test]
    fn single_line_joins_paragraphs() {
        assert_eq!(single_line("One.\n\nTwo.", Some("en")), "One. Two.");
        assert_eq!(single_line("One.\n\nTwo.", None), "One. Two.");
        assert_eq!(single_line("一つ。\n\n二つ。", Some("ja")), "一つ。二つ。");
        assert_eq!(single_line("No breaks", Some("ja")), "No breaks");
    }

    #[test]
    fn default_export_file_name_replaces_extension_and_sanitizes() {
        use TranscriptFormat::*;
        assert_eq!(
            default_export_file_name("interview.final.m4a", Srt),
            "interview.final.srt"
        );
        assert_eq!(default_export_file_name("memo", Txt), "memo.txt");
        assert_eq!(
            default_export_file_name("a<b>c:d\"e|f?g*h.wav", Md),
            "a_b_c_d_e_f_g_h.md"
        );
        assert_eq!(
            default_export_file_name("tab\there.mp3", Vtt),
            "tab_here.vtt"
        );
        assert_eq!(
            default_export_file_name("dir.v2/clip", Json),
            "dir.v2_clip.json"
        );
        assert_eq!(default_export_file_name("", Txt), "transcript.txt");
        assert_eq!(default_export_file_name("  .wav", Srt), "transcript.srt");
        assert_eq!(default_export_file_name("...wav", Srt), "transcript.srt");
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_atomic_creates_and_replaces_without_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.srt");

        write_atomic(&path, "first\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\n");
        write_atomic(&path, "second\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
        assert_eq!(dir_entries(dir.path()), vec!["transcript.srt"]);
    }

    #[test]
    fn write_atomic_failure_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // The rename fails because the target is a folder.
        let target = dir.path().join("taken");
        fs::create_dir(&target).unwrap();
        assert!(write_atomic(&target, "text").is_err());
        assert_eq!(dir_entries(dir.path()), vec!["taken"]);

        assert!(write_atomic(&dir.path().join("missing").join("x.txt"), "text").is_err());
        assert!(write_atomic(&dir.path().join(".."), "text").is_err());
        assert_eq!(dir_entries(dir.path()), vec!["taken"]);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_writes_through_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let synced = dir.path().join("synced");
        fs::create_dir(&synced).unwrap();
        fs::write(synced.join("real.srt"), "old\n").unwrap();
        // Relative targets resolve from the link's folder, through chains.
        symlink("synced/real.srt", dir.path().join("hop.srt")).unwrap();
        symlink("hop.srt", dir.path().join("link.srt")).unwrap();

        write_atomic(&dir.path().join("link.srt"), "new\n").unwrap();

        assert_eq!(
            fs::read_to_string(synced.join("real.srt")).unwrap(),
            "new\n"
        );
        for link in ["link.srt", "hop.srt"] {
            let metadata = fs::symlink_metadata(dir.path().join(link)).unwrap();
            assert!(metadata.file_type().is_symlink(), "{link}");
        }
        assert_eq!(dir_entries(&synced), vec!["real.srt"]);

        // A dangling link creates its target.
        symlink(synced.join("later.txt"), dir.path().join("dangling.txt")).unwrap();
        write_atomic(&dir.path().join("dangling.txt"), "made\n").unwrap();
        assert_eq!(
            fs::read_to_string(synced.join("later.txt")).unwrap(),
            "made\n"
        );
        assert!(fs::symlink_metadata(dir.path().join("dangling.txt"))
            .unwrap()
            .file_type()
            .is_symlink());

        // A link loop is an error, not a hang.
        symlink("loop-b", dir.path().join("loop-a")).unwrap();
        symlink("loop-a", dir.path().join("loop-b")).unwrap();
        assert!(write_atomic(&dir.path().join("loop-a"), "x").is_err());
        assert!(check_writable(&dir.path().join("loop-a")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_the_replaced_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.txt");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&path, "new\n").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_writes_into_pipes_and_devices() {
        use std::io::Read;
        use std::os::unix::fs::{symlink, FileTypeExt};
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("out.fifo");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        // Like `-o >(gzip > t.gz)`: nothing may open the pipe early.
        check_writable(&fifo).unwrap();
        let reader = {
            let fifo = fifo.clone();
            std::thread::spawn(move || {
                let mut text = String::new();
                File::open(fifo).unwrap().read_to_string(&mut text).unwrap();
                text
            })
        };
        write_atomic(&fifo, "through the pipe\n").unwrap();

        assert_eq!(reader.join().unwrap(), "through the pipe\n");
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
        assert_eq!(dir_entries(dir.path()), vec!["out.fifo"]);

        // Like `-o /dev/stdout`: the device is written, the link kept.
        let null_link = dir.path().join("null");
        symlink("/dev/null", &null_link).unwrap();
        check_writable(&null_link).unwrap();
        write_atomic(&null_link, "discarded\n").unwrap();
        assert!(fs::symlink_metadata(&null_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::metadata("/dev/null")
            .unwrap()
            .file_type()
            .is_char_device());
        assert_eq!(dir_entries(dir.path()), vec!["null", "out.fifo"]);
    }

    #[test]
    fn check_writable_tries_the_folder_without_leaving_files() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("old.txt");
        fs::write(&existing, "keep\n").unwrap();

        check_writable(&dir.path().join("new.srt")).unwrap();
        check_writable(&existing).unwrap();
        assert_eq!(fs::read_to_string(&existing).unwrap(), "keep\n");
        assert_eq!(dir_entries(dir.path()), vec!["old.txt"]);

        assert!(check_writable(&dir.path().join("missing").join("x.txt")).is_err());
        // A file where the folder should be.
        assert!(check_writable(&existing.join("x.txt")).is_err());
        assert!(check_writable(&dir.path().join("..")).is_err());
        assert_eq!(dir_entries(dir.path()), vec!["old.txt"]);
    }
}
