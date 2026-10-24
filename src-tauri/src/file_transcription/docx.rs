//! Word (.docx) export: a small, deterministic WordprocessingML package
//! written by hand. The same transcript and options always give the same
//! bytes: the parts come in a fixed order with a fixed ZIP date, and the
//! only machine-dependent value (the local time of "Transcribed") comes in
//! through `ExportOptions`.
//!
//! Word offers to "repair" a document whose elements are out of schema
//! order, so the children written here follow ISO/IEC 29500 order:
//!
//! - `w:pPr`: pStyle < pBdr < bidi < spacing < jc
//! - `w:rPr`: rStyle < rFonts < b < bCs < color < sz < szCs < rtl < lang
//! - `w:style`: name < basedOn < next < uiPriority < semiHidden <
//!   unhideWhenUsed < qFormat < pPr < rPr
//! - `w:sectPr`: footerReference < pgSz < pgMar
//! - `w:settings`: defaultTabStop < characterSpacingControl < compat
//! - `w:styles`: docDefaults (rPrDefault < pPrDefault) < style*
//!
//! A test checks every part against these tables.
//!
//! Every user string (file name, model, language, speaker names, status
//! detail, transcript text) goes through `clean`, then `lines` or
//! `one_line`, then `xml_escape`, so no character XML forbids reaches a part.

use crate::file_transcription::export::{format_clock, ExportOptions};
use crate::file_transcription::speakers::speaker_label;
use crate::file_transcription::types::{FileTranscript, TranscriptSpeakers, TranscriptStatus};
use chrono::{DateTime, Datelike, FixedOffset, Offset, Utc};
use flate2::write::DeflateEncoder;
use flate2::{Compression, Crc};
use std::io::{self, Write};

/// A part: the XML declaration every part starts with, then `$body`.
macro_rules! xml_part {
    ($body:literal) => {
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n",
            $body
        )
    };
}

const DECLARATION: &str = xml_part!("");

const CONTENT_TYPES: &str = xml_part!(
    "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
     <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
     <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
     <Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
     <Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
     <Override PartName=\"/word/settings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml\"/>\
     <Override PartName=\"/word/footer1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml\"/>\
     <Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
     </Types>"
);

const PACKAGE_RELS: &str = xml_part!(
    "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
     <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
     <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
     </Relationships>"
);

const DOCUMENT_RELS: &str = xml_part!(
    "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
     <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
     <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings\" Target=\"settings.xml\"/>\
     <Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer\" Target=\"footer1.xml\"/>\
     </Relationships>"
);

/// compatibilityMode 15 keeps Word from opening the file in "[Compatibility
/// Mode]". No updateFields: it makes Word ask a question on every open.
const SETTINGS: &str = xml_part!(
    "<w:settings xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
     <w:defaultTabStop w:val=\"720\"/>\
     <w:characterSpacingControl w:val=\"doNotCompress\"/>\
     <w:compat><w:compatSetting w:name=\"compatibilityMode\" w:uri=\"http://schemas.microsoft.com/office/word\" w:val=\"15\"/></w:compat>\
     </w:settings>"
);

/// The page number, centred (the Footer style).
const FOOTER: &str = xml_part!(
    "<w:ftr xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
     <w:p><w:pPr><w:pStyle w:val=\"Footer\"/></w:pPr>\
     <w:fldSimple w:instr=\" PAGE \"><w:r><w:t>1</w:t></w:r></w:fldSimple></w:p>\
     </w:ftr>"
);

const DOCUMENT_START: &str = "<w:document \
     xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
     xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
     <w:body>";

/// US Letter, 1-inch margins, the page-number footer.
const DOCUMENT_END: &str = "<w:sectPr>\
     <w:footerReference w:type=\"default\" r:id=\"rId3\"/>\
     <w:pgSz w:w=\"12240\" w:h=\"15840\"/>\
     <w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/>\
     </w:sectPr></w:body></w:document>";

const DETAILS_START: &str = "<w:p><w:pPr><w:pStyle w:val=\"TranscriptDetails\"/></w:pPr>";

/// The last details line closes the header with a thin rule and 12 pt.
const LAST_DETAILS_START: &str = "<w:p><w:pPr><w:pStyle w:val=\"TranscriptDetails\"/>\
     <w:pBdr><w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"6\" w:color=\"auto\"/></w:pBdr>\
     <w:spacing w:after=\"240\"/></w:pPr>";

const BOLD: &str = "<w:b/><w:bCs/>";
const RTL: &str = "<w:rtl/>";
const TIME_STYLE: &str = "<w:rStyle w:val=\"TranscriptTime\"/>";
const SPEAKER_STYLE: &str = "<w:rStyle w:val=\"TranscriptSpeaker\"/>";

/// The transcript as a Word document: the bytes of a .docx file.
pub fn render_docx(t: &FileTranscript, options: &ExportOptions) -> io::Result<Vec<u8>> {
    let mut zip = ZipWriter::new();
    zip.add("[Content_Types].xml", CONTENT_TYPES.as_bytes())?;
    zip.add("_rels/.rels", PACKAGE_RELS.as_bytes())?;
    zip.add("docProps/core.xml", core_xml(t).as_bytes())?;
    zip.add("word/document.xml", document_xml(t, options).as_bytes())?;
    zip.add("word/_rels/document.xml.rels", DOCUMENT_RELS.as_bytes())?;
    zip.add(
        "word/styles.xml",
        styles_xml(t.language.as_deref()).as_bytes(),
    )?;
    zip.add("word/settings.xml", SETTINGS.as_bytes())?;
    zip.add("word/footer1.xml", FOOTER.as_bytes())?;
    zip.finish()
}

/// word/document.xml: the title, the details lines, one paragraph per
/// non-blank transcript paragraph, then the page setup.
pub(crate) fn document_xml(t: &FileTranscript, options: &ExportOptions) -> String {
    let mut out = String::with_capacity(4_096 + 2 * t.text.len());
    out.push_str(DECLARATION);
    out.push_str(DOCUMENT_START);

    let title = document_title(t);
    let title_bidi = mostly_rtl(&title);
    out.push_str("<w:p><w:pPr><w:pStyle w:val=\"Title\"/>");
    if title_bidi {
        out.push_str("<w:bidi/>");
    }
    out.push_str("</w:pPr>");
    if title_bidi {
        push_rtl_runs(&mut out, &[("", &title)]);
    } else {
        push_run(&mut out, "", &[&title]);
    }
    out.push_str("</w:p>");

    // Labels and values are short, mostly Latin text: never right-to-left.
    let details = details(t, options);
    let last = details.len().saturating_sub(1);
    for (i, (label, value)) in details.iter().enumerate() {
        out.push_str(if i == last {
            LAST_DETAILS_START
        } else {
            DETAILS_START
        });
        push_run(&mut out, BOLD, &[format!("{label}: ")]);
        push_run(&mut out, "", &[value]);
        out.push_str("</w:p>");
    }

    if t.paragraphs.is_empty() {
        // Only reachable for a transcript stored without paragraph timing:
        // keep its text, untimed, as Markdown does.
        for paragraph in t.text.split("\n\n") {
            push_body_paragraph(&mut out, None, None, paragraph);
        }
    }
    for paragraph in &t.paragraphs {
        let label = match (paragraph.speaker, &t.speakers) {
            (Some(id), Some(speakers)) => Some(display_label(speakers, id)),
            _ => None,
        };
        push_body_paragraph(
            &mut out,
            Some(paragraph.start_ms),
            label.as_deref(),
            &paragraph.text,
        );
    }

    out.push_str(DOCUMENT_END);
    out
}

/// word/styles.xml. Calibri 11 pt (Arial for complex scripts), the Title,
/// Footer and three transcript styles. For Japanese, Chinese and Korean the
/// defaults name the East Asian language, which steers Word's font and
/// proofing choice; a wrong guess is harmless.
pub(crate) fn styles_xml(language: Option<&str>) -> String {
    let lang = east_asian_lang(language)
        .map(|tag| format!("<w:lang w:eastAsia=\"{tag}\"/>"))
        .unwrap_or_default();
    format!(
        "{DECLARATION}<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
         <w:docDefaults>\
         <w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Calibri\" w:hAnsi=\"Calibri\" w:cs=\"Arial\"/>\
         <w:sz w:val=\"22\"/><w:szCs w:val=\"22\"/>{lang}</w:rPr></w:rPrDefault>\
         <w:pPrDefault><w:pPr><w:spacing w:after=\"160\" w:line=\"259\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault>\
         </w:docDefaults>\
         <w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\">\
         <w:name w:val=\"Normal\"/><w:qFormat/></w:style>\
         <w:style w:type=\"character\" w:default=\"1\" w:styleId=\"DefaultParagraphFont\">\
         <w:name w:val=\"Default Paragraph Font\"/><w:uiPriority w:val=\"1\"/><w:semiHidden/><w:unhideWhenUsed/></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"Title\">\
         <w:name w:val=\"Title\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:uiPriority w:val=\"10\"/><w:qFormat/>\
         <w:pPr><w:spacing w:after=\"120\"/></w:pPr>\
         <w:rPr><w:b/><w:bCs/><w:sz w:val=\"36\"/><w:szCs w:val=\"36\"/></w:rPr></w:style>\
         <w:style w:type=\"paragraph\" w:customStyle=\"1\" w:styleId=\"TranscriptDetails\">\
         <w:name w:val=\"Transcript Details\"/><w:basedOn w:val=\"Normal\"/><w:uiPriority w:val=\"11\"/><w:qFormat/>\
         <w:pPr><w:spacing w:after=\"0\"/></w:pPr></w:style>\
         <w:style w:type=\"character\" w:customStyle=\"1\" w:styleId=\"TranscriptTime\">\
         <w:name w:val=\"Transcript Time\"/><w:basedOn w:val=\"DefaultParagraphFont\"/><w:uiPriority w:val=\"1\"/><w:qFormat/>\
         <w:rPr><w:color w:val=\"595959\"/></w:rPr></w:style>\
         <w:style w:type=\"character\" w:customStyle=\"1\" w:styleId=\"TranscriptSpeaker\">\
         <w:name w:val=\"Transcript Speaker\"/><w:basedOn w:val=\"DefaultParagraphFont\"/><w:uiPriority w:val=\"1\"/><w:qFormat/>\
         <w:rPr><w:b/><w:bCs/></w:rPr></w:style>\
         <w:style w:type=\"paragraph\" w:styleId=\"Footer\">\
         <w:name w:val=\"footer\"/><w:basedOn w:val=\"Normal\"/><w:uiPriority w:val=\"99\"/><w:unhideWhenUsed/>\
         <w:pPr><w:spacing w:after=\"0\" w:line=\"240\" w:lineRule=\"auto\"/><w:jc w:val=\"center\"/></w:pPr>\
         <w:rPr><w:color w:val=\"595959\"/><w:sz w:val=\"18\"/><w:szCs w:val=\"18\"/></w:rPr></w:style>\
         </w:styles>"
    )
}

/// docProps/core.xml: the title (File > Info) and, when W3CDTF can show it
/// with a four-digit year, `created_at` (UTC) as created and modified. No
/// creator.
fn core_xml(t: &FileTranscript) -> String {
    let dates = DateTime::from_timestamp(t.created_at, 0)
        .filter(|created| (1..=9999).contains(&created.year()))
        .map(|created| {
            let date = created.format("%Y-%m-%dT%H:%M:%SZ");
            format!(
                "<dcterms:created xsi:type=\"dcterms:W3CDTF\">{date}</dcterms:created>\
                 <dcterms:modified xsi:type=\"dcterms:W3CDTF\">{date}</dcterms:modified>"
            )
        })
        .unwrap_or_default();
    format!(
        "{DECLARATION}<cp:coreProperties \
         xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:dcterms=\"http://purl.org/dc/terms/\" \
         xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
         <dc:title>{}</dc:title>{dates}</cp:coreProperties>",
        xml_escape(&document_title(t))
    )
}

/// The file name on one line, or "Transcript" when nothing is left of it.
fn document_title(t: &FileTranscript) -> String {
    let title = one_line(&t.source_name);
    if title.is_empty() {
        "Transcript".to_string()
    } else {
        title
    }
}

/// The details lines, as (English label, value), like the Markdown header.
fn details(t: &FileTranscript, options: &ExportOptions) -> Vec<(&'static str, String)> {
    let mut details = vec![("Duration", format_clock(t.duration_ms))];
    let model = t
        .model_name
        .as_deref()
        .map(one_line)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| one_line(&t.model_id));
    if !model.is_empty() {
        details.push(("Model", model));
    }
    if let Some(language) = t
        .language
        .as_deref()
        .map(one_line)
        .filter(|language| !language.is_empty())
    {
        details.push(("Language", language));
    }
    if let Some(speakers) = &t.speakers {
        let names: Vec<String> = speakers
            .speakers
            .iter()
            .map(|speaker| display_label(speakers, speaker.id))
            .collect();
        if !names.is_empty() {
            details.push(("Speakers", names.join(", ")));
        }
    }
    if let Some(transcribed) = transcribed(t.created_at, options.utc_offset_secs) {
        details.push(("Transcribed", transcribed));
    }
    if t.status == TranscriptStatus::Partial {
        let status = match t
            .status_detail
            .as_deref()
            .map(one_line)
            .filter(|detail| !detail.is_empty())
        {
            Some(detail) => format!("Incomplete — {detail}"),
            None => "Incomplete".to_string(),
        };
        details.push(("Status", status));
    }
    details
}

/// "November 14, 2023, 2:13 PM": `created_at` at `utc_offset_secs` from UTC
/// (UTC when the offset is not a valid one). None when out of range.
fn transcribed(created_at: i64, utc_offset_secs: i32) -> Option<String> {
    let offset = FixedOffset::east_opt(utc_offset_secs).unwrap_or(Utc.fix());
    let created = DateTime::from_timestamp(created_at, 0)?;
    Some(
        created
            .with_timezone(&offset)
            .format("%B %-d, %Y, %-I:%M %p")
            .to_string(),
    )
}

/// A speaker's label on one line ("Doug", "Speaker 2"); "Speaker N" when
/// nothing is left of the name once cleaned.
fn display_label(speakers: &TranscriptSpeakers, id: u32) -> String {
    let label = one_line(&speaker_label(speakers, id));
    if label.is_empty() {
        format!("Speaker {id}")
    } else {
        label
    }
}

/// One transcript paragraph: `[HH:MM:SS] ` (when timed) in the Transcript
/// Time style, so hiding that style hides the times and their spaces; the
/// speaker's `Label: ` in the Transcript Speaker style; then the text, its
/// lines joined by line breaks. Skipped when the text has no lines.
///
/// Direction is decided per paragraph from its own letters, as the app's
/// view does: a mixed-language transcript keeps its English paragraphs
/// left-to-right. Inside a right-to-left paragraph, the speaker and text
/// are cut into runs by script (see `rtl_flags`), so an English phrase
/// keeps its word order.
fn push_body_paragraph(out: &mut String, start_ms: Option<u64>, label: Option<&str>, text: &str) {
    let lines = lines(text);
    if lines.is_empty() {
        return;
    }
    let bidi = mostly_rtl(text);
    out.push_str("<w:p>");
    if bidi {
        out.push_str("<w:pPr><w:bidi/></w:pPr>");
    }
    if let Some(start_ms) = start_ms {
        push_run(out, TIME_STYLE, &[format!("[{}] ", format_clock(start_ms))]);
    }
    let label = label.map(|label| format!("{label}: "));
    if bidi {
        // One pass over the label and the text: a space or colon between
        // them is laid out by the letters on both sides.
        let text = lines.join("\n");
        let mut parts = Vec::with_capacity(2);
        if let Some(label) = &label {
            parts.push((SPEAKER_STYLE, label.as_str()));
        }
        parts.push(("", text.as_str()));
        push_rtl_runs(out, &parts);
    } else {
        if let Some(label) = &label {
            push_run(out, SPEAKER_STYLE, &[label]);
        }
        push_run(out, "", &lines);
    }
    out.push_str("</w:p>");
}

/// One run: `properties` (the children of `w:rPr`, in schema order; none
/// when empty), then `lines` separated by line breaks (an empty line writes
/// only its break). The lines must already be cleaned; they are escaped
/// here.
fn push_run<S: AsRef<str>>(out: &mut String, properties: &str, lines: &[S]) {
    out.push_str("<w:r>");
    if !properties.is_empty() {
        out.push_str("<w:rPr>");
        out.push_str(properties);
        out.push_str("</w:rPr>");
    }
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("<w:br/>");
        }
        let line = line.as_ref();
        if !line.is_empty() {
            out.push_str("<w:t xml:space=\"preserve\">");
            out.push_str(&xml_escape(line));
            out.push_str("</w:t>");
        }
    }
    out.push_str("</w:r>");
}

/// The runs of a right-to-left paragraph: each of `parts` (the run
/// properties, then its cleaned text, lines separated by '\n') cut where the
/// direction changes, with `<w:rtl/>` added to the right-to-left pieces.
/// The direction is worked out over all the parts together.
fn push_rtl_runs(out: &mut String, parts: &[(&str, &str)]) {
    let chars: Vec<char> = parts.iter().flat_map(|(_, text)| text.chars()).collect();
    let mut flags = rtl_flags(&chars).into_iter();
    for (properties, text) in parts {
        let part_flags = flags.by_ref().take(text.chars().count());
        for (rtl, piece) in split_by_direction(text, part_flags) {
            let properties = if rtl {
                format!("{properties}{RTL}")
            } else {
                (*properties).to_string()
            };
            let lines: Vec<&str> = piece.split('\n').collect();
            push_run(out, &properties, &lines);
        }
    }
}

/// `text` cut where `flags` (one per character) changes, as
/// (right-to-left, piece). A line break stays with the piece before it: it
/// reads the same in either direction.
fn split_by_direction(text: &str, flags: impl Iterator<Item = bool>) -> Vec<(bool, String)> {
    let mut pieces: Vec<(bool, String)> = Vec::new();
    for (c, rtl) in text.chars().zip(flags) {
        match pieces.last_mut() {
            Some((last, piece)) if *last == rtl || c == '\n' => piece.push(c),
            _ => pieces.push((rtl, c.to_string())),
        }
    }
    pieces
}

/// How a character of a right-to-left paragraph decides its run's
/// direction.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strength {
    Ltr,
    Rtl,
    Neutral,
}

/// For each of `chars` (a right-to-left paragraph's text, '\n' between
/// lines), whether it goes in a run marked `<w:rtl/>`.
///
/// Word reads `<w:rtl/>` as an override (ISO/IEC 29500 17.3.2.30): inside
/// such a run every space and punctuation mark is laid out right to left,
/// so "Microsoft Office" in one shows as "Office Microsoft". So, as Word
/// does when someone types the text, only right-to-left letters are marked,
/// with the characters the Unicode bidirectional algorithm would lay out
/// right to left anyway:
///
/// - a combining accent goes with the character before it;
/// - digits after a left-to-right letter belong to it ("Windows 10");
/// - spaces, punctuation and line breaks between two left-to-right letters
///   stay left-to-right; all the others (between two right-to-left letters,
///   between the two scripts, at either end) and digits after a
///   right-to-left letter are right-to-left, the paragraph's direction.
fn rtl_flags(chars: &[char]) -> Vec<bool> {
    let mut strengths: Vec<Strength> = Vec::with_capacity(chars.len());
    // The paragraph's direction stands before its first letter.
    let mut last_strong = Strength::Rtl;
    for &c in chars {
        let strength = if is_rtl_char(c) {
            Strength::Rtl
        } else if c.is_alphabetic() {
            Strength::Ltr
        } else if is_combining_mark(c) {
            strengths.last().copied().unwrap_or(Strength::Neutral)
        } else if c.is_numeric() && last_strong == Strength::Ltr {
            Strength::Ltr
        } else {
            Strength::Neutral
        };
        if strength != Strength::Neutral {
            last_strong = strength;
        }
        strengths.push(strength);
    }

    let mut flags = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < strengths.len() {
        let end = match strengths[i] {
            Strength::Neutral => strengths[i..]
                .iter()
                .position(|&strength| strength != Strength::Neutral)
                .map_or(strengths.len(), |n| i + n),
            _ => i + 1,
        };
        let rtl = match strengths[i] {
            Strength::Ltr => false,
            Strength::Rtl => true,
            Strength::Neutral => {
                let before = i.checked_sub(1).and_then(|j| strengths.get(j)).copied();
                let after = strengths.get(end).copied();
                !(before == Some(Strength::Ltr) && after == Some(Strength::Ltr))
            }
        };
        flags.extend(std::iter::repeat_n(rtl, end - i));
        i = end;
    }
    flags
}

/// Combining accents and variation selectors, which belong to the
/// character before them (a file name from macOS spells "é" as "e" and an
/// accent).
fn is_combining_mark(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0xE0100..=0xE01EF
    )
}

/// Keeps only characters XML allows: a tab becomes a space, every kind of
/// line break becomes '\n', other control characters and U+FFFE/U+FFFF are
/// dropped.
fn clean(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}' => Some('\n'),
            '\u{FFFE}' | '\u{FFFF}' => None,
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// The cleaned lines of `text`, trimmed, without blank ones.
fn lines(text: &str) -> Vec<String> {
    clean(text)
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The cleaned lines of `text` joined by spaces.
fn one_line(text: &str) -> String {
    lines(text).join(" ")
}

/// Escapes text for an element or a double-quoted attribute.
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// More of the letters of `text` are right-to-left than left-to-right (a
/// tie is left-to-right). Digits, punctuation and spaces don't count.
fn mostly_rtl(text: &str) -> bool {
    let (mut rtl, mut ltr) = (0usize, 0usize);
    for c in text.chars().filter(|c| c.is_alphabetic()) {
        if is_rtl_char(c) {
            rtl += 1;
        } else {
            ltr += 1;
        }
    }
    rtl > ltr
}

/// Hebrew, Arabic, Syriac, Thaana, N'Ko and the other right-to-left blocks,
/// with their presentation forms.
fn is_rtl_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF
    )
}

/// The East Asian language tag for Word's defaults: Japanese, Korean,
/// Cantonese, and Chinese (traditional for Taiwan, Hong Kong, Macau and
/// "Hant", simplified otherwise).
fn east_asian_lang(language: Option<&str>) -> Option<&'static str> {
    let language = language?.trim().to_ascii_lowercase().replace('_', "-");
    let mut subtags = language.split('-');
    match subtags.next()? {
        "ja" => Some("ja-JP"),
        "ko" => Some("ko-KR"),
        "yue" => Some("zh-HK"),
        "zh" if subtags.any(|tag| matches!(tag, "tw" | "hant" | "hk" | "mo")) => Some("zh-TW"),
        "zh" => Some("zh-CN"),
        _ => None,
    }
}

fn too_large() -> io::Error {
    io::Error::other("the transcript is too large for a Word document")
}

/// A minimal ZIP writer: deflated (or stored) entries with a fixed date,
/// no ZIP64, data descriptors, extra fields or comments.
struct ZipWriter {
    /// Local headers and entry data.
    out: Vec<u8>,
    /// The central directory, written after the last entry.
    central: Vec<u8>,
    entries: u16,
}

impl ZipWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            central: Vec::new(),
            entries: 0,
        }
    }

    /// Adds `data` as `name` (ASCII), deflated unless that does not make it
    /// smaller.
    fn add(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        let mut crc = Crc::new();
        crc.update(data);
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data)?;
        let deflated = encoder.finish()?;
        let (method, body): (u16, &[u8]) = if deflated.len() < data.len() {
            (8, &deflated)
        } else {
            (0, data)
        };

        let offset = u32::try_from(self.out.len()).map_err(|_| too_large())?;
        let compressed_size = u32::try_from(body.len()).map_err(|_| too_large())?;
        let size = u32::try_from(data.len()).map_err(|_| too_large())?;
        let name_len = u16::try_from(name.len()).map_err(|_| too_large())?;
        self.entries = self.entries.checked_add(1).ok_or_else(too_large)?;

        // Shared by the local header and the central directory entry:
        // version needed (2.0), flags, method, time and date (1980-01-01
        // 00:00), CRC-32, sizes, name length, extra field length.
        let mut common = Vec::with_capacity(26);
        common.extend_from_slice(&20u16.to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes());
        common.extend_from_slice(&method.to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes());
        common.extend_from_slice(&0x0021u16.to_le_bytes());
        common.extend_from_slice(&crc.sum().to_le_bytes());
        common.extend_from_slice(&compressed_size.to_le_bytes());
        common.extend_from_slice(&size.to_le_bytes());
        common.extend_from_slice(&name_len.to_le_bytes());
        common.extend_from_slice(&0u16.to_le_bytes());

        self.out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        self.out.extend_from_slice(&common);
        self.out.extend_from_slice(name.as_bytes());
        self.out.extend_from_slice(body);

        self.central
            .extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        // Made by: MS-DOS, ZIP 2.0.
        self.central.extend_from_slice(&20u16.to_le_bytes());
        self.central.extend_from_slice(&common);
        // Comment length, disk number, internal and external attributes.
        self.central.extend_from_slice(&0u16.to_le_bytes());
        self.central.extend_from_slice(&0u16.to_le_bytes());
        self.central.extend_from_slice(&0u16.to_le_bytes());
        self.central.extend_from_slice(&0u32.to_le_bytes());
        self.central.extend_from_slice(&offset.to_le_bytes());
        self.central.extend_from_slice(name.as_bytes());
        Ok(())
    }

    /// The archive: the entries, the central directory and its end record.
    fn finish(mut self) -> io::Result<Vec<u8>> {
        let central_offset = u32::try_from(self.out.len()).map_err(|_| too_large())?;
        let central_size = u32::try_from(self.central.len()).map_err(|_| too_large())?;
        self.out.extend_from_slice(&self.central);
        self.out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        // This disk, the disk where the central directory starts.
        self.out.extend_from_slice(&0u16.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes());
        // Entries on this disk, entries in total.
        self.out.extend_from_slice(&self.entries.to_le_bytes());
        self.out.extend_from_slice(&self.entries.to_le_bytes());
        self.out.extend_from_slice(&central_size.to_le_bytes());
        self.out.extend_from_slice(&central_offset.to_le_bytes());
        // Comment length.
        self.out.extend_from_slice(&0u16.to_le_bytes());
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::export::tests::{
        empty, labelled_sample, labelled_seg, sample, seg,
    };
    use crate::file_transcription::types::TranscriptSpeaker;
    use chrono::{Local, TimeZone};
    use std::io::Read;

    const DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
    const PST: ExportOptions = ExportOptions {
        utc_offset_secs: -8 * 3600,
    };
    const PART_NAMES: [&str; 8] = [
        "[Content_Types].xml",
        "_rels/.rels",
        "docProps/core.xml",
        "word/document.xml",
        "word/_rels/document.xml.rels",
        "word/styles.xml",
        "word/settings.xml",
        "word/footer1.xml",
    ];
    const JAPANESE: &str = "今日はお越しいただきありがとうございます。直角の「直」と骨。";

    // ---- Fixtures -------------------------------------------------------

    /// labelled_sample's speakers, renamed: ids 1.. in order.
    fn speakers(names: &[Option<&str>]) -> Option<TranscriptSpeakers> {
        let mut speakers = labelled_sample().speakers?;
        speakers.speakers = names
            .iter()
            .zip(1..)
            .map(|(name, id)| TranscriptSpeaker {
                id,
                name: name.map(str::to_string),
            })
            .collect();
        Some(speakers)
    }

    fn hebrew() -> FileTranscript {
        FileTranscript {
            source_name: "ראיון.m4a".to_string(),
            language: Some("he".to_string()),
            paragraphs: vec![
                labelled_seg(0, 5_000, "OK, שלום וברוך הבא. כמה שעות אתה עומד?", 1),
                labelled_seg(5_000, 9_000, "כמעט כל המשמרת, 8 שעות.", 2),
            ],
            speakers: speakers(&[Some("דוד"), None]),
            ..labelled_sample()
        }
    }

    /// Mostly Hebrew and Arabic paragraphs holding English names and terms,
    /// as Whisper writes them; the file name's "é" is an "e" and an accent,
    /// as macOS spells it.
    fn english_in_rtl() -> FileTranscript {
        FileTranscript {
            source_name: "Jose\u{301} ראיון חדש.m4a".to_string(),
            language: Some("he".to_string()),
            paragraphs: vec![
                labelled_seg(
                    0,
                    4_000,
                    "צריך ללמוד את Microsoft Office לפני הראיון בעבודה.",
                    2,
                ),
                labelled_seg(
                    4_000,
                    8_000,
                    "يعمل كـ forklift operator في المستودع منذ خمس سنوات",
                    2,
                ),
                labelled_seg(8_000, 12_000, "עבדתי עם Windows 10 ו-Office 365 שנתיים.", 2),
                labelled_seg(
                    12_000,
                    16_000,
                    "Home Depot היא החברה שבה עבדתי שנתיים\nבמחלקת Customer\nService, ועכשיו Microsoft\nשוב.",
                    1,
                ),
            ],
            speakers: speakers(&[Some("Doug"), None]),
            ..labelled_sample()
        }
    }

    /// Detected as Arabic overall, but two of three paragraphs are English.
    fn mixed() -> FileTranscript {
        FileTranscript {
            language: Some("ar".to_string()),
            paragraphs: vec![
                labelled_seg(
                    0,
                    5_000,
                    "Thanks for coming in today. How long are you usually standing?",
                    1,
                ),
                labelled_seg(5_000, 9_000, "شكرا لك. أقف حوالي ثماني ساعات، OK؟", 2),
                labelled_seg(
                    9_000,
                    12_000,
                    "Did you say eight hours, as דוד wrote down?",
                    1,
                ),
            ],
            ..labelled_sample()
        }
    }

    fn japanese() -> FileTranscript {
        FileTranscript {
            language: Some("ja".to_string()),
            paragraphs: vec![seg(0, 5_000, JAPANESE)],
            ..sample()
        }
    }

    const NASTY: &str =
        "x & y < z > \"w\" \u{0}\u{8}\u{B}\u{1F}\u{7F}\u{85}\u{9F}\u{FFFE}\u{FFFF} end";

    fn escaping() -> FileTranscript {
        FileTranscript {
            source_name: "A<b>&c \"q\"\u{0}\u{8}.m4a".to_string(),
            speakers: speakers(&[Some("A<b>&c"), Some("line\nbreak")]),
            paragraphs: vec![
                labelled_seg(0, 1_000, NASTY, 1),
                labelled_seg(
                    1_000,
                    2_000,
                    "line one\r\nline two\n\n  line three\u{2028}four\tTab",
                    2,
                ),
                labelled_seg(2_000, 3_000, "   \n  \u{1}\u{7}  ", 2),
            ],
            status: TranscriptStatus::Partial,
            status_detail: Some(" \u{1} ".to_string()),
            ..labelled_sample()
        }
    }

    /// Stored without paragraph timing: only `text`.
    fn untimed() -> FileTranscript {
        FileTranscript {
            paragraphs: Vec::new(),
            ..sample()
        }
    }

    fn fixtures() -> Vec<(&'static str, FileTranscript)> {
        vec![
            ("sample", sample()),
            ("labelled", labelled_sample()),
            ("hebrew", hebrew()),
            ("english_in_rtl", english_in_rtl()),
            ("mixed", mixed()),
            ("japanese", japanese()),
            ("escaping", escaping()),
            ("empty", empty()),
            ("untimed", untimed()),
        ]
    }

    // ---- Helpers --------------------------------------------------------

    struct Entry {
        name: String,
        method: u16,
        data: Vec<u8>,
    }

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// Reads the archive back through its central directory, checking each
    /// local header against its central entry and every size and CRC.
    fn unzip(zip: &[u8]) -> Vec<Entry> {
        let end = zip.len() - 22;
        assert_eq!(u32_at(zip, end), 0x0605_4b50, "end of central directory");
        assert_eq!(
            (u16_at(zip, end + 4), u16_at(zip, end + 6)),
            (0, 0),
            "disks"
        );
        let count = u16_at(zip, end + 8);
        assert_eq!(u16_at(zip, end + 10), count);
        let central_size = u32_at(zip, end + 12) as usize;
        let central_offset = u32_at(zip, end + 16) as usize;
        assert_eq!(u16_at(zip, end + 20), 0, "archive comment");
        assert_eq!(central_offset + central_size, end);

        let mut entries = Vec::new();
        let mut at = central_offset;
        let mut next_local = 0;
        for _ in 0..count {
            let central = &zip[at..];
            assert_eq!(u32_at(central, 0), 0x0201_4b50, "central entry");
            assert_eq!(u16_at(central, 4), 20, "made by");
            assert_eq!(u16_at(central, 6), 20, "version needed");
            assert_eq!(u16_at(central, 8), 0, "flags");
            let method = u16_at(central, 10);
            assert!(method == 0 || method == 8, "method {method}");
            assert_eq!(u16_at(central, 12), 0, "time");
            assert_eq!(u16_at(central, 14), 0x0021, "date");
            let crc = u32_at(central, 16);
            let compressed_size = u32_at(central, 20) as usize;
            let size = u32_at(central, 24) as usize;
            let name_len = u16_at(central, 28) as usize;
            assert_eq!(u16_at(central, 30), 0, "extra field");
            assert_eq!(u16_at(central, 32), 0, "comment");
            assert_eq!(u16_at(central, 34), 0, "disk");
            assert_eq!(u16_at(central, 36), 0, "internal attributes");
            assert_eq!(u32_at(central, 38), 0, "external attributes");
            let offset = u32_at(central, 42) as usize;
            let name = std::str::from_utf8(&central[46..46 + name_len]).unwrap();

            assert_eq!(offset, next_local, "{name}: entries are contiguous");
            let local = &zip[offset..];
            assert_eq!(u32_at(local, 0), 0x0403_4b50, "{name}: local header");
            assert_eq!(local[4..8], central[6..10], "{name}");
            assert_eq!(local[8..30], central[10..32], "{name}");
            assert_eq!(&local[30..30 + name_len], name.as_bytes());
            let body = &local[30 + name_len..30 + name_len + compressed_size];
            let data = if method == 8 {
                let mut data = Vec::new();
                flate2::read::DeflateDecoder::new(body)
                    .read_to_end(&mut data)
                    .unwrap();
                data
            } else {
                body.to_vec()
            };
            assert_eq!(data.len(), size, "{name}: size");
            let mut actual = Crc::new();
            actual.update(&data);
            assert_eq!(actual.sum(), crc, "{name}: CRC");

            next_local = offset + 30 + name_len + compressed_size;
            at += 46 + name_len;
            entries.push(Entry {
                name: name.to_string(),
                method,
                data,
            });
        }
        assert_eq!(next_local, central_offset);
        assert_eq!(at, end);
        entries
    }

    /// The parts of the rendered package, as text.
    fn parts(t: &FileTranscript, options: &ExportOptions) -> Vec<(String, String)> {
        unzip(&render_docx(t, options).unwrap())
            .into_iter()
            .map(|entry| (entry.name, String::from_utf8(entry.data).unwrap()))
            .collect()
    }

    /// Every `<w:p>…</w:p>`, in order (paragraphs never nest here).
    fn paragraphs(xml: &str) -> Vec<&str> {
        xml.match_indices("<w:p>")
            .map(|(start, _)| {
                let end = start + xml[start..].find("</w:p>").unwrap() + "</w:p>".len();
                &xml[start..end]
            })
            .collect()
    }

    /// The transcript paragraphs: those without a paragraph style.
    fn body_paragraphs(xml: &str) -> Vec<&str> {
        paragraphs(xml)
            .into_iter()
            .filter(|paragraph| !paragraph.contains("<w:pStyle "))
            .collect()
    }

    /// The runs (`<w:r>…</w:r>`) of a paragraph.
    fn runs(paragraph: &str) -> Vec<&str> {
        paragraph
            .split("<w:r>")
            .skip(1)
            .map(|run| &run[..run.find("</w:r>").unwrap()])
            .collect()
    }

    /// Each run of a paragraph as (marked `<w:rtl/>`, its text with '\n' for
    /// a line break). The text stays escaped.
    fn run_texts(paragraph: &str) -> Vec<(bool, String)> {
        runs(paragraph)
            .into_iter()
            .map(|run| {
                let mut text = String::new();
                for tag in run.split('<').skip(1) {
                    if tag.starts_with("w:br/>") {
                        text.push('\n');
                    } else if let Some(t) = tag.strip_prefix("w:t xml:space=\"preserve\">") {
                        text.push_str(t);
                    }
                }
                (run.contains("<w:rtl/>"), text)
            })
            .collect()
    }

    /// (right-to-left, text) pieces, from string literals.
    fn expect(pieces: &[(bool, &str)]) -> Vec<(bool, String)> {
        pieces
            .iter()
            .map(|&(rtl, text)| (rtl, text.to_string()))
            .collect()
    }

    /// The values of every `name="…"` attribute.
    fn attribute_values<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
        let pattern = format!(" {name}=\"");
        xml.match_indices(&pattern)
            .map(|(start, _)| {
                let value = &xml[start + pattern.len()..];
                &value[..value.find('"').unwrap()]
            })
            .collect()
    }

    fn is_xml_char(c: char) -> bool {
        matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
    }

    /// Text or an attribute value: only the five predefined entities, and
    /// no `<`.
    fn check_text(text: &str) -> Result<(), String> {
        if text.contains('<') {
            return Err(format!("raw '<' in {text:?}"));
        }
        for (i, _) in text.match_indices('&') {
            let rest = &text[i..];
            if !["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"]
                .iter()
                .any(|entity| rest.starts_with(entity))
            {
                return Err(format!("raw '&' in {text:?}"));
            }
        }
        Ok(())
    }

    /// A small well-formedness check for the subset of XML written here:
    /// the declaration, one root, balanced tags, double-quoted attributes,
    /// the five predefined entities and only characters XML 1.0 allows.
    fn check_well_formed(xml: &str) -> Result<(), String> {
        let mut rest = xml.strip_prefix(DECL).ok_or("no XML declaration")?;
        if let Some(c) = xml.chars().find(|&c| !is_xml_char(c)) {
            return Err(format!("forbidden character U+{:04X}", u32::from(c)));
        }
        let mut open: Vec<&str> = Vec::new();
        let mut roots = 0;
        while !rest.is_empty() {
            let Some(tag) = rest.strip_prefix('<') else {
                if open.is_empty() {
                    return Err("text outside the root element".to_string());
                }
                let end = rest.find('<').unwrap_or(rest.len());
                check_text(&rest[..end])?;
                rest = &rest[end..];
                continue;
            };
            if let Some(tag) = tag.strip_prefix('/') {
                let end = tag.find('>').ok_or("unterminated end tag")?;
                if open.pop() != Some(&tag[..end]) {
                    return Err(format!("mismatched </{}>", &tag[..end]));
                }
                rest = &tag[end + 1..];
                continue;
            }
            let name_end = tag
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .ok_or("unterminated tag")?;
            let name = &tag[..name_end];
            if name.is_empty() || name.contains(['<', '&', '"', '=']) {
                return Err(format!("bad element name {name:?}"));
            }
            if open.is_empty() {
                roots += 1;
            }
            let mut attributes = &tag[name_end..];
            loop {
                attributes = attributes.trim_start();
                if let Some(after) = attributes.strip_prefix("/>") {
                    rest = after;
                    break;
                }
                if let Some(after) = attributes.strip_prefix('>') {
                    open.push(name);
                    rest = after;
                    break;
                }
                let eq = attributes.find('=').ok_or("attribute without a value")?;
                let attribute = &attributes[..eq];
                if attribute.is_empty()
                    || attribute.contains(|c: char| c.is_whitespace() || "<>&\"/".contains(c))
                {
                    return Err(format!("bad attribute name {attribute:?}"));
                }
                let value = attributes[eq + 1..]
                    .strip_prefix('"')
                    .ok_or("unquoted attribute")?;
                let close = value.find('"').ok_or("unterminated attribute")?;
                check_text(&value[..close])?;
                attributes = &value[close + 1..];
            }
        }
        match (open.last(), roots) {
            (Some(name), _) => Err(format!("unclosed <{name}>")),
            (None, 1) => Ok(()),
            (None, n) => Err(format!("{n} root elements")),
        }
    }

    /// Children every checked element may have, in schema order.
    const ORDER: &[(&str, &[&str])] = &[
        (
            "w:pPr",
            &["w:pStyle", "w:pBdr", "w:bidi", "w:spacing", "w:jc"],
        ),
        (
            "w:rPr",
            &[
                "w:rStyle", "w:rFonts", "w:b", "w:bCs", "w:color", "w:sz", "w:szCs", "w:rtl",
                "w:lang",
            ],
        ),
        (
            "w:style",
            &[
                "w:name",
                "w:basedOn",
                "w:next",
                "w:uiPriority",
                "w:semiHidden",
                "w:unhideWhenUsed",
                "w:qFormat",
                "w:pPr",
                "w:rPr",
            ],
        ),
        ("w:sectPr", &["w:footerReference", "w:pgSz", "w:pgMar"]),
        (
            "w:settings",
            &["w:defaultTabStop", "w:characterSpacingControl", "w:compat"],
        ),
        ("w:docDefaults", &["w:rPrDefault", "w:pPrDefault"]),
    ];

    /// (element, its direct children) for every element, from a scan of
    /// the tags. Text never holds a raw '<' (see check_well_formed).
    fn children_of_each(xml: &str) -> Vec<(String, Vec<String>)> {
        let mut done = Vec::new();
        let mut open: Vec<(String, Vec<String>)> = Vec::new();
        for tag in xml.strip_prefix(DECL).unwrap().split('<').skip(1) {
            let tag = &tag[..tag.find('>').unwrap()];
            if let Some(name) = tag.strip_prefix('/') {
                let (element, children) = open.pop().unwrap();
                assert_eq!(element, name);
                done.push((element, children));
                continue;
            }
            let name = tag
                .split(|c: char| c.is_whitespace() || c == '/')
                .next()
                .unwrap()
                .to_string();
            if let Some((_, children)) = open.last_mut() {
                children.push(name.clone());
            }
            if tag.ends_with('/') {
                done.push((name, Vec::new()));
            } else {
                open.push((name, Vec::new()));
            }
        }
        done
    }

    /// Checks the children of every element named in ORDER (and of
    /// `w:styles`); returns the names of the elements checked.
    fn check_schema_order(part: &str, xml: &str) -> Vec<String> {
        let mut checked = Vec::new();
        for (element, children) in children_of_each(xml) {
            if element == "w:styles" {
                assert_eq!(children[0], "w:docDefaults", "{part}");
                assert!(children[1..].iter().all(|c| c == "w:style"), "{part}");
                checked.push(element);
                continue;
            }
            let Some((_, order)) = ORDER.iter().find(|(name, _)| *name == element) else {
                continue;
            };
            let ranks: Vec<usize> = children
                .iter()
                .map(|child| {
                    order
                        .iter()
                        .position(|name| name == child)
                        .unwrap_or_else(|| panic!("{part}: <{child}> in <{element}>"))
                })
                .collect();
            assert!(
                ranks.windows(2).all(|pair| pair[0] < pair[1]),
                "{part}: <{element}> children out of order: {children:?}"
            );
            checked.push(element);
        }
        checked
    }

    // ---- Fixed parts ----------------------------------------------------

    const STYLES_BEFORE_LANG: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        "\n",
        r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Arial"/><w:sz w:val="22"/><w:szCs w:val="22"/>"#,
    );
    const STYLES_AFTER_LANG: &str = concat!(
        r#"</w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="259" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults>"#,
        r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style>"#,
        r#"<w:style w:type="character" w:default="1" w:styleId="DefaultParagraphFont"><w:name w:val="Default Paragraph Font"/><w:uiPriority w:val="1"/><w:semiHidden/><w:unhideWhenUsed/></w:style>"#,
        r#"<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="10"/><w:qFormat/><w:pPr><w:spacing w:after="120"/></w:pPr><w:rPr><w:b/><w:bCs/><w:sz w:val="36"/><w:szCs w:val="36"/></w:rPr></w:style>"#,
        r#"<w:style w:type="paragraph" w:customStyle="1" w:styleId="TranscriptDetails"><w:name w:val="Transcript Details"/><w:basedOn w:val="Normal"/><w:uiPriority w:val="11"/><w:qFormat/><w:pPr><w:spacing w:after="0"/></w:pPr></w:style>"#,
        r#"<w:style w:type="character" w:customStyle="1" w:styleId="TranscriptTime"><w:name w:val="Transcript Time"/><w:basedOn w:val="DefaultParagraphFont"/><w:uiPriority w:val="1"/><w:qFormat/><w:rPr><w:color w:val="595959"/></w:rPr></w:style>"#,
        r#"<w:style w:type="character" w:customStyle="1" w:styleId="TranscriptSpeaker"><w:name w:val="Transcript Speaker"/><w:basedOn w:val="DefaultParagraphFont"/><w:uiPriority w:val="1"/><w:qFormat/><w:rPr><w:b/><w:bCs/></w:rPr></w:style>"#,
        r#"<w:style w:type="paragraph" w:styleId="Footer"><w:name w:val="footer"/><w:basedOn w:val="Normal"/><w:uiPriority w:val="99"/><w:unhideWhenUsed/><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/><w:jc w:val="center"/></w:pPr><w:rPr><w:color w:val="595959"/><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr></w:style></w:styles>"#,
    );

    #[test]
    fn fixed_parts_golden() {
        let cases = [
            (
                CONTENT_TYPES,
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/word/settings.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.settings+xml"/><Override PartName="/word/footer1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/></Types>"#,
            ),
            (
                PACKAGE_RELS,
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/></Relationships>"#,
            ),
            (
                DOCUMENT_RELS,
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer" Target="footer1.xml"/></Relationships>"#,
            ),
            (
                SETTINGS,
                r#"<w:settings xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:defaultTabStop w:val="720"/><w:characterSpacingControl w:val="doNotCompress"/><w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/></w:compat></w:settings>"#,
            ),
            (
                FOOTER,
                r#"<w:ftr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:pPr><w:pStyle w:val="Footer"/></w:pPr><w:fldSimple w:instr=" PAGE "><w:r><w:t>1</w:t></w:r></w:fldSimple></w:p></w:ftr>"#,
            ),
        ];
        for (part, body) in cases {
            assert_eq!(part, format!("{DECL}{body}"));
        }
        assert_eq!(DECLARATION, DECL);

        assert_eq!(
            styles_xml(None),
            format!("{STYLES_BEFORE_LANG}{STYLES_AFTER_LANG}")
        );
        assert_eq!(
            styles_xml(Some("ja")),
            format!("{STYLES_BEFORE_LANG}<w:lang w:eastAsia=\"ja-JP\"/>{STYLES_AFTER_LANG}")
        );

        let core = concat!(
            r#"<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#,
            r#"<dc:title>interview.m4a</dc:title>"#,
        );
        assert_eq!(
            core_xml(&sample()),
            format!(
                "{DECL}{core}{}{}</cp:coreProperties>",
                r#"<dcterms:created xsi:type="dcterms:W3CDTF">2023-11-14T22:13:20Z</dcterms:created>"#,
                r#"<dcterms:modified xsi:type="dcterms:W3CDTF">2023-11-14T22:13:20Z</dcterms:modified>"#,
            )
        );
        // Year 10000 and beyond: W3CDTF has four digits for the year.
        for created_at in [253_402_300_800, i64::MAX] {
            let t = FileTranscript {
                created_at,
                ..sample()
            };
            assert_eq!(
                core_xml(&t),
                format!("{DECL}{core}</cp:coreProperties>"),
                "{created_at}"
            );
        }
        let last_second = FileTranscript {
            created_at: 253_402_300_799,
            ..sample()
        };
        assert!(core_xml(&last_second).contains(">9999-12-31T23:59:59Z</dcterms:created>"));
    }

    // ---- word/document.xml ----------------------------------------------

    const LABELLED_DOCUMENT: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        "\n",
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t xml:space="preserve">interview.m4a</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Duration: </w:t></w:r><w:r><w:t xml:space="preserve">00:00:10</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Model: </w:t></w:r><w:r><w:t xml:space="preserve">Whisper Small</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Language: </w:t></w:r><w:r><w:t xml:space="preserve">en</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Speakers: </w:t></w:r><w:r><w:t xml:space="preserve">Doug, Speaker 2</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/><w:pBdr><w:bottom w:val="single" w:sz="4" w:space="6" w:color="auto"/></w:pBdr><w:spacing w:after="240"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Transcribed: </w:t></w:r><w:r><w:t xml:space="preserve">November 14, 2023, 2:13 PM</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:rPr><w:rStyle w:val="TranscriptTime"/></w:rPr><w:t xml:space="preserve">[00:00:00] </w:t></w:r><w:r><w:rPr><w:rStyle w:val="TranscriptSpeaker"/></w:rPr><w:t xml:space="preserve">Doug: </w:t></w:r><w:r><w:t xml:space="preserve">Thanks for coming in today. How long are you usually standing?</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:rPr><w:rStyle w:val="TranscriptTime"/></w:rPr><w:t xml:space="preserve">[00:00:08] </w:t></w:r><w:r><w:rPr><w:rStyle w:val="TranscriptSpeaker"/></w:rPr><w:t xml:space="preserve">Speaker 2: </w:t></w:r><w:r><w:t xml:space="preserve">Almost the whole shift.</w:t></w:r></w:p>"#,
        r#"<w:sectPr><w:footerReference w:type="default" r:id="rId3"/><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr></w:body></w:document>"#,
    );

    const SAMPLE_DOCUMENT: &str = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
        "\n",
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t xml:space="preserve">interview.m4a</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Duration: </w:t></w:r><w:r><w:t xml:space="preserve">01:02:05</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Model: </w:t></w:r><w:r><w:t xml:space="preserve">Whisper Small</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Language: </w:t></w:r><w:r><w:t xml:space="preserve">en</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Transcribed: </w:t></w:r><w:r><w:t xml:space="preserve">November 14, 2023, 10:13 PM</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:pStyle w:val="TranscriptDetails"/><w:pBdr><w:bottom w:val="single" w:sz="4" w:space="6" w:color="auto"/></w:pBdr><w:spacing w:after="240"/></w:pPr><w:r><w:rPr><w:b/><w:bCs/></w:rPr><w:t xml:space="preserve">Status: </w:t></w:r><w:r><w:t xml:space="preserve">Incomplete — Decoding stopped early at 00:41:10</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:rPr><w:rStyle w:val="TranscriptTime"/></w:rPr><w:t xml:space="preserve">[00:00:00] </w:t></w:r><w:r><w:t xml:space="preserve">Hello and welcome to the show. Today we talk about transcription.</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:rPr><w:rStyle w:val="TranscriptTime"/></w:rPr><w:t xml:space="preserve">[00:00:27] </w:t></w:r><w:r><w:t xml:space="preserve">Second paragraph text.</w:t></w:r></w:p>"#,
        r#"<w:sectPr><w:footerReference w:type="default" r:id="rId3"/><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr></w:body></w:document>"#,
    );

    #[test]
    fn document_golden() {
        assert_eq!(document_xml(&labelled_sample(), &PST), LABELLED_DOCUMENT);
        assert_eq!(
            document_xml(&sample(), &ExportOptions::default()),
            SAMPLE_DOCUMENT
        );
    }

    #[test]
    fn transcribed_is_local_time_at_the_given_offset() {
        let cases = [
            (0, "November 14, 2023, 10:13 PM"),
            (-8 * 3600, "November 14, 2023, 2:13 PM"),
            (9 * 3600, "November 15, 2023, 7:13 AM"),
            (5 * 3600 + 30 * 60, "November 15, 2023, 3:43 AM"),
        ];
        for (utc_offset_secs, expected) in cases {
            let xml = document_xml(&sample(), &ExportOptions { utc_offset_secs });
            let line = format!(
                "Transcribed: </w:t></w:r><w:r><w:t xml:space=\"preserve\">{expected}</w:t>"
            );
            assert!(xml.contains(&line), "{utc_offset_secs}: {xml}");
        }

        let created_at = 1_700_000_000;
        let local = Local
            .timestamp_opt(created_at, 0)
            .single()
            .unwrap()
            .offset()
            .local_minus_utc();
        assert_eq!(
            ExportOptions::local(created_at),
            ExportOptions {
                utc_offset_secs: local
            }
        );
        assert_eq!(ExportOptions::local(i64::MAX), ExportOptions::default());

        // Out of range: no "Transcribed" line, no dates.
        let t = FileTranscript {
            created_at: i64::MAX,
            ..sample()
        };
        assert!(!document_xml(&t, &PST).contains("Transcribed"));
    }

    #[test]
    fn user_text_is_cleaned_and_escaped() {
        let t = escaping();
        let xml = document_xml(&t, &PST);
        let body = body_paragraphs(&xml);
        assert_eq!(body.len(), 2, "the blank paragraph is skipped: {xml}");

        let nasty_run = runs(body[0])[2];
        assert_eq!(
            nasty_run,
            "<w:t xml:space=\"preserve\">x &amp; y &lt; z &gt; &quot;w&quot;</w:t>\
             <w:br/><w:t xml:space=\"preserve\">end</w:t>"
        );
        let lines_run = runs(body[1])[2];
        assert_eq!(
            lines_run,
            "<w:t xml:space=\"preserve\">line one</w:t><w:br/>\
             <w:t xml:space=\"preserve\">line two</w:t><w:br/>\
             <w:t xml:space=\"preserve\">line three</w:t><w:br/>\
             <w:t xml:space=\"preserve\">four Tab</w:t>"
        );

        // Names: escaped in the details and the body, on one line.
        assert!(xml.contains(">A&lt;b&gt;&amp;c, line break</w:t>"), "{xml}");
        assert!(body[0].contains(">A&lt;b&gt;&amp;c: </w:t>"), "{xml}");
        assert!(body[1].contains(">line break: </w:t>"), "{xml}");
        // The title and the blank status detail.
        assert!(
            xml.contains(">A&lt;b&gt;&amp;c &quot;q&quot;.m4a</w:t>"),
            "{xml}"
        );
        assert!(
            xml.contains("Status: </w:t></w:r><w:r><w:t xml:space=\"preserve\">Incomplete</w:t>")
        );
        assert!(core_xml(&t).contains("<dc:title>A&lt;b&gt;&amp;c &quot;q&quot;.m4a</dc:title>"));

        // A name with nothing left but control characters.
        let untitled = FileTranscript {
            source_name: "\u{1}\u{2}".to_string(),
            ..empty()
        };
        assert!(document_xml(&untitled, &PST).contains(
            "<w:pStyle w:val=\"Title\"/></w:pPr><w:r><w:t xml:space=\"preserve\">Transcript</w:t>"
        ));
        assert!(core_xml(&untitled).contains("<dc:title>Transcript</dc:title>"));
        let unnamed = FileTranscript {
            speakers: speakers(&[Some("\u{1}"), None]),
            ..labelled_sample()
        };
        assert!(document_xml(&unnamed, &PST).contains(">Speaker 1, Speaker 2</w:t>"));

        assert_eq!(clean("a\tb\u{0C}c\u{2029}d\u{7}"), "a b\nc\nd");
        assert_eq!(lines(" \n a \r\n\n b \u{85}"), ["a", "b"]);
        assert_eq!(one_line(" Two\nLines "), "Two Lines");
        assert_eq!(xml_escape("<'&\">"), "&lt;'&amp;&quot;&gt;");
    }

    #[test]
    fn direction_is_decided_per_paragraph() {
        let xml = document_xml(&hebrew(), &PST);
        assert!(xml.contains(
            "<w:p><w:pPr><w:pStyle w:val=\"Title\"/><w:bidi/></w:pPr><w:r><w:rPr><w:rtl/></w:rPr>"
        ));
        let body = body_paragraphs(&xml);
        assert_eq!(body.len(), 2);
        for paragraph in &body {
            assert!(paragraph.starts_with("<w:p><w:pPr><w:bidi/></w:pPr>"));
            assert!(
                runs(paragraph)[0]
                    .starts_with("<w:rPr><w:rStyle w:val=\"TranscriptTime\"/></w:rPr>"),
                "time: {paragraph}"
            );
        }
        // "OK, …" starts the first paragraph, which is still Hebrew; the
        // speaker's name is Hebrew too.
        assert_eq!(
            run_texts(body[0]),
            expect(&[
                (false, "[00:00:00] "),
                (true, "דוד: "),
                (false, "OK"),
                (true, ", שלום וברוך הבא. כמה שעות אתה עומד?"),
            ])
        );
        assert!(runs(body[0])[1]
            .starts_with("<w:rPr><w:rStyle w:val=\"TranscriptSpeaker\"/><w:rtl/></w:rPr>"));
        // "Speaker 2" is English; its colon reads with the Hebrew.
        assert_eq!(
            run_texts(body[1]),
            expect(&[
                (false, "[00:00:05] "),
                (false, "Speaker 2"),
                (true, ": "),
                (true, "כמעט כל המשמרת, 8 שעות."),
            ])
        );
        assert!(
            runs(body[1])[1].starts_with("<w:rPr><w:rStyle w:val=\"TranscriptSpeaker\"/></w:rPr>")
        );
        assert!(runs(body[1])[2]
            .starts_with("<w:rPr><w:rStyle w:val=\"TranscriptSpeaker\"/><w:rtl/></w:rPr>"));

        // The transcript's language does not decide: only Arabic is RTL.
        let xml = document_xml(&mixed(), &PST);
        let body = body_paragraphs(&xml);
        assert_eq!(body.len(), 3);
        assert!(!body[0].contains("<w:bidi/>") && !body[0].contains("<w:rtl/>"));
        assert!(body[1].starts_with("<w:p><w:pPr><w:bidi/></w:pPr>"));
        assert_eq!(
            run_texts(body[1]),
            expect(&[
                (false, "[00:00:05] "),
                (false, "Speaker 2"),
                (true, ": "),
                (true, "شكرا لك. أقف حوالي ثماني ساعات، "),
                (false, "OK"),
                (true, "؟"),
            ])
        );
        // An English paragraph quoting a Hebrew name.
        assert!(body[2].contains("דוד"));
        assert!(!body[2].contains("<w:bidi/>") && !body[2].contains("<w:rtl/>"));

        // Details are never right-to-left.
        for xml in [document_xml(&hebrew(), &PST), document_xml(&mixed(), &PST)] {
            for paragraph in paragraphs(&xml) {
                if paragraph.contains("TranscriptDetails") {
                    assert!(!paragraph.contains("<w:bidi/>") && !paragraph.contains("<w:rtl/>"));
                }
            }
        }

        assert!(mostly_rtl("OK, שלום"));
        assert!(!mostly_rtl("ab שׁ"), "a tie is left-to-right");
        assert!(!mostly_rtl("123 ... !"));
        assert!(mostly_rtl("مرحبا"));
    }

    /// Word lays out every space and punctuation mark of an `<w:rtl/>` run
    /// right to left, so a two-word English phrase in one would show its
    /// words swapped ("Office Microsoft"): English stretches, with the
    /// spaces and digits inside them, get runs of their own.
    #[test]
    fn english_phrases_in_rtl_paragraphs_keep_their_word_order() {
        let xml = document_xml(&english_in_rtl(), &PST);
        let body = body_paragraphs(&xml);
        assert_eq!(body.len(), 4);
        for paragraph in &body {
            assert!(paragraph.starts_with("<w:p><w:pPr><w:bidi/></w:pPr>"));
        }
        let cases: [&[(bool, &str)]; 4] = [
            &[
                (false, "[00:00:00] "),
                (false, "Speaker 2"),
                (true, ": "),
                (true, "צריך ללמוד את "),
                (false, "Microsoft Office"),
                (true, " לפני הראיון בעבודה."),
            ],
            &[
                (false, "[00:00:04] "),
                (false, "Speaker 2"),
                (true, ": "),
                (true, "يعمل كـ "),
                (false, "forklift operator"),
                (true, " في المستودع منذ خمس سنوات"),
            ],
            // Digits after an English word belong to it.
            &[
                (false, "[00:00:08] "),
                (false, "Speaker 2"),
                (true, ": "),
                (true, "עבדתי עם "),
                (false, "Windows 10"),
                (true, " ו-"),
                (false, "Office 365"),
                (true, " שנתיים."),
            ],
            // An English name before English text stays English, colon and
            // all. A line break between English words is English; one after
            // an English word stays in its run.
            &[
                (false, "[00:00:12] "),
                (false, "Doug: "),
                (false, "Home Depot"),
                (true, " היא החברה שבה עבדתי שנתיים\nבמחלקת "),
                (false, "Customer\nService"),
                (true, ", ועכשיו "),
                (false, "Microsoft\n"),
                (true, "שוב."),
            ],
        ];
        for (paragraph, expected) in body.iter().zip(cases) {
            assert_eq!(run_texts(paragraph), expect(expected), "{paragraph}");
        }
        // "Microsoft\n" is the run's text, then its break: no empty text.
        assert!(xml.contains(">Microsoft</w:t><w:br/></w:r>"), "{xml}");
        assert!(!xml.contains("<w:t xml:space=\"preserve\"></w:t>"), "{xml}");
        // The speaker style is kept on both pieces of a cut label.
        let runs = runs(body[0]);
        assert!(runs[1].starts_with("<w:rPr><w:rStyle w:val=\"TranscriptSpeaker\"/></w:rPr>"));
        assert!(
            runs[2].starts_with("<w:rPr><w:rStyle w:val=\"TranscriptSpeaker\"/><w:rtl/></w:rPr>")
        );
        assert!(runs[4].starts_with("<w:t "), "no properties: {}", runs[4]);

        // The title too; the accent stays with its "e".
        let title = paragraphs(&xml)[0];
        assert!(title.starts_with("<w:p><w:pPr><w:pStyle w:val=\"Title\"/><w:bidi/></w:pPr>"));
        assert_eq!(
            run_texts(title),
            expect(&[
                (false, "Jose\u{301}"),
                (true, " ראיון חדש."),
                (false, "m4a")
            ])
        );
    }

    #[test]
    fn rtl_flags_follow_the_letters_around_each_space() {
        let pieces = |text: &str| {
            let chars: Vec<char> = text.chars().collect();
            split_by_direction(text, rtl_flags(&chars).into_iter())
        };
        // Nothing but neutrals: the paragraph's direction.
        assert_eq!(pieces("12 ... !"), expect(&[(true, "12 ... !")]));
        assert_eq!(pieces(""), expect(&[]));
        // Ends take the paragraph's direction; English punctuation and
        // digits between English letters stay English.
        assert_eq!(
            pieces("(check-in, 9:30 AM) עם don't"),
            expect(&[
                (true, "("),
                (false, "check-in, 9:30 AM"),
                (true, ") עם "),
                (false, "don't"),
            ])
        );
        // Digits after Hebrew read with it, even before English.
        assert_eq!(
            pieces("בשעה 5:30 PM"),
            expect(&[(true, "בשעה 5:30 "), (false, "PM")])
        );
        // An accent with nothing before it is neutral.
        assert_eq!(pieces("\u{301}שלום"), expect(&[(true, "\u{301}שלום")]));
    }

    #[test]
    fn east_asian_languages_get_a_language_hint() {
        let cases = [
            (Some("ja"), Some("ja-JP")),
            (Some("zh"), Some("zh-CN")),
            (Some("zh-CN"), Some("zh-CN")),
            (Some("zh-TW"), Some("zh-TW")),
            (Some("zh_Hant"), Some("zh-TW")),
            (Some("zh-HK"), Some("zh-TW")),
            (Some("zh-mo"), Some("zh-TW")),
            (Some("yue"), Some("zh-HK")),
            (Some("ko"), Some("ko-KR")),
            (Some("en"), None),
            (Some("he"), None),
            (Some(""), None),
            (None, None),
        ];
        for (language, expected) in cases {
            assert_eq!(east_asian_lang(language), expected, "{language:?}");
            let styles = styles_xml(language);
            match expected {
                Some(tag) => assert!(
                    styles.contains(&format!(
                        "<w:szCs w:val=\"22\"/><w:lang w:eastAsia=\"{tag}\"/></w:rPr>"
                    )),
                    "{language:?}"
                ),
                None => assert!(!styles.contains("<w:lang "), "{language:?}"),
            }
        }
        // The text itself is unchanged: Word wraps it.
        let xml = document_xml(&japanese(), &PST);
        assert!(xml.contains(&format!("<w:t xml:space=\"preserve\">{JAPANESE}</w:t>")));
    }

    #[test]
    fn edge_cases() {
        // Nothing said: the title and details only.
        let xml = document_xml(&empty(), &PST);
        assert!(body_paragraphs(&xml).is_empty(), "{xml}");
        assert!(xml.contains("</w:t></w:r></w:p><w:sectPr>"));

        // Partial without a detail.
        let t = FileTranscript {
            status_detail: None,
            ..sample()
        };
        assert!(document_xml(&t, &PST)
            .contains("Status: </w:t></w:r><w:r><w:t xml:space=\"preserve\">Incomplete</w:t>"));

        // No paragraph timing: the text's paragraphs, untimed.
        let xml = document_xml(&untimed(), &PST);
        assert_eq!(
            body_paragraphs(&xml),
            [
                "<w:p><w:r><w:t xml:space=\"preserve\">Hello and welcome to the show. Today we talk about transcription.</w:t></w:r></w:p>",
                "<w:p><w:r><w:t xml:space=\"preserve\">Second paragraph text.</w:t></w:r></w:p>",
            ]
        );

        // An unlabelled paragraph in a labelled transcript, and a split
        // monologue that repeats its speaker.
        let mut t = labelled_sample();
        t.paragraphs[1].speaker = None;
        t.paragraphs
            .push(labelled_seg(11_000, 12_000, "And another thing.", 1));
        t.paragraphs
            .push(labelled_seg(12_000, 13_000, "One more.", 1));
        let xml = document_xml(&t, &PST);
        let body = body_paragraphs(&xml);
        assert_eq!(
            body[1],
            "<w:p><w:r><w:rPr><w:rStyle w:val=\"TranscriptTime\"/></w:rPr>\
             <w:t xml:space=\"preserve\">[00:00:08] </w:t></w:r>\
             <w:r><w:t xml:space=\"preserve\">Almost the whole shift.</w:t></w:r></w:p>"
        );
        for paragraph in [body[2], body[3]] {
            assert!(paragraph.contains(">Doug: </w:t>"), "{paragraph}");
        }

        // Model falls back to its id; blank values are left out.
        let t = FileTranscript {
            model_name: Some(" \n ".to_string()),
            language: Some(" ".to_string()),
            ..empty()
        };
        let xml = document_xml(&t, &PST);
        assert!(xml.contains("Model: </w:t></w:r><w:r><w:t xml:space=\"preserve\">small</w:t>"));
        assert!(!xml.contains("Language"));
        let t = FileTranscript {
            model_name: None,
            model_id: String::new(),
            ..empty()
        };
        assert!(!document_xml(&t, &PST).contains("Model"));
    }

    // ---- The package ----------------------------------------------------

    #[test]
    fn every_part_is_well_formed_and_in_schema_order() {
        let mut checked: Vec<String> = Vec::new();
        for (fixture, t) in fixtures() {
            for (name, xml) in parts(&t, &PST) {
                let part = format!("{fixture}: {name}");
                assert_eq!(check_well_formed(&xml), Ok(()), "{part}");
                checked.extend(check_schema_order(&part, &xml));
            }
        }
        for element in [
            "w:pPr",
            "w:rPr",
            "w:style",
            "w:sectPr",
            "w:settings",
            "w:docDefaults",
            "w:styles",
        ] {
            assert!(checked.iter().any(|c| c == element), "{element} not seen");
        }
    }

    #[test]
    fn well_formedness_check_rejects_broken_xml() {
        let ok = format!("{DECL}<a x=\"1 &amp; 2\"><b/>text &lt;&gt;<c></c></a>");
        assert_eq!(check_well_formed(&ok), Ok(()));
        for broken in [
            "<a><b></a></b>",
            "<a>x & y</a>",
            "<a>\u{1}</a>",
            "<a><b></b>",
            "<a b=\"x/>",
            "<a b=x/>",
            "<a/><b/>",
            "text<a/>",
        ] {
            assert!(
                check_well_formed(&format!("{DECL}{broken}")).is_err(),
                "{broken:?}"
            );
        }
        assert!(check_well_formed("<a/>").is_err(), "no declaration");
    }

    #[test]
    fn archive_lists_the_parts_in_order_and_round_trips() {
        for (fixture, t) in fixtures() {
            let entries = unzip(&render_docx(&t, &PST).unwrap());
            let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
            assert_eq!(names, PART_NAMES, "{fixture}");
            assert!(entries.iter().all(|entry| entry.method == 8), "{fixture}");

            let text = |name: &str| {
                let entry = entries.iter().find(|entry| entry.name == name).unwrap();
                String::from_utf8(entry.data.clone()).unwrap()
            };
            assert_eq!(text("word/document.xml"), document_xml(&t, &PST));
            assert_eq!(text("word/styles.xml"), styles_xml(t.language.as_deref()));
            assert_eq!(text("docProps/core.xml"), core_xml(&t));

            // Every relationship and content-type override names a part.
            for rels in ["_rels/.rels", "word/_rels/document.xml.rels"] {
                let base = &rels[..rels.find("_rels/").unwrap()];
                for target in attribute_values(&text(rels), "Target") {
                    let part = format!("{base}{target}");
                    assert!(names.contains(&part.as_str()), "{fixture}: {part}");
                }
            }
            for part in attribute_values(&text("[Content_Types].xml"), "PartName") {
                assert!(names.contains(&&part[1..]), "{fixture}: {part}");
            }
        }
    }

    #[test]
    fn rendering_is_deterministic() {
        let t = labelled_sample();
        let first = render_docx(&t, &PST).unwrap();
        assert_eq!(first, render_docx(&t, &PST).unwrap());

        let tokyo = ExportOptions {
            utc_offset_secs: 9 * 3600,
        };
        let pacific = unzip(&first);
        let other = unzip(&render_docx(&t, &tokyo).unwrap());
        for (a, b) in pacific.iter().zip(&other) {
            assert_eq!(
                a.data == b.data,
                a.name != "word/document.xml",
                "{}",
                a.name
            );
        }
    }

    #[test]
    fn zip_writer_stores_what_deflate_cannot_shrink() {
        // xorshift32: incompressible bytes.
        let mut state = 0x2545_f491u32;
        let noise: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect();
        let mut zip = ZipWriter::new();
        zip.add("noise.bin", &noise).unwrap();
        let entries = unzip(&zip.finish().unwrap());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].method, 0);
        assert_eq!(entries[0].data, noise);

        let repetitive = "So how long were you on your feet? ".repeat(30_000);
        let repetitive = &repetitive.as_bytes()[..1_000_000];
        let mut zip = ZipWriter::new();
        zip.add("text.xml", repetitive).unwrap();
        let archive = zip.finish().unwrap();
        assert!(archive.len() < repetitive.len() / 20, "{}", archive.len());
        let entries = unzip(&archive);
        assert_eq!(entries[0].method, 8);
        assert_eq!(entries[0].data, repetitive);

        let empty = ZipWriter::new().finish().unwrap();
        assert!(unzip(&empty).is_empty());
    }
}
