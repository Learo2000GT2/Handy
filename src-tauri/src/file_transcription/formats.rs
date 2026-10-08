//! Which decoder handles which file, and the list shown to users.
//!
//! A file with a known extension is routed by its extension only: symphonia
//! 0.5.5 ignores the extension hint and probes for 2-byte MPEG sync markers,
//! so letting it sniff the content would misidentify some of the ffmpeg-only
//! formats as MP3. A file with no or an unknown extension (a temp file, say)
//! is routed by the few magic numbers at its start that cannot be mistaken
//! for anything else.

use crate::file_transcription::{ffmpeg, types::SupportedAudioFormats};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeRoute {
    Symphonia,
    Ffmpeg,
}

/// Routed to symphonia (WAV first tries the fast path in decode.rs).
pub const SYMPHONIA_EXTENSIONS: &[&str] = &[
    "wav", "wave", "aif", "aiff", "aifc", "mp3", "mp2", "mp1", "mpga", "flac", "ogg", "oga", "m4a",
    "m4b", "mp4", "m4v", "mov", "3gp", "aac",
];
/// Routed straight to ffmpeg. Symphonia's 2-byte MP3 sync markers would mis-probe some of these as MPEG audio.
pub const FFMPEG_EXTENSIONS: &[&str] = &[
    "opus", "webm", "mkv", "mka", "wma", "asf", "wmv", "amr", "awb", "caf", "ac3", "eac3", "avi",
    "flv", "ts", "mts", "m2ts", "mpg", "mpeg", "vob", "dss", "spx", "wv", "ape", "3g2", "mxf",
];

/// The file's extension in lower case, without the dot.
pub(crate) fn lowercase_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
}

/// Case-insensitive extension routing; a file without a known extension is
/// routed by its content (`sniff`). None = unsupported.
pub fn route_for_path(path: &Path) -> Option<DecodeRoute> {
    match lowercase_extension(path)
        .as_deref()
        .and_then(route_for_extension)
    {
        Some(route) => Some(route),
        None => sniff(path).map(|sniffed| sniffed.route),
    }
}

fn route_for_extension(ext: &str) -> Option<DecodeRoute> {
    if SYMPHONIA_EXTENSIONS.contains(&ext) {
        Some(DecodeRoute::Symphonia)
    } else if FFMPEG_EXTENSIONS.contains(&ext) {
        Some(DecodeRoute::Ffmpeg)
    } else {
        None
    }
}

/// The extension a file is handled as: its own when that is a known one,
/// otherwise that of the format `sniff` recognised.
pub(crate) fn format_extension(path: &Path) -> Option<String> {
    match lowercase_extension(path) {
        Some(ext) if route_for_extension(&ext).is_some() => Some(ext),
        _ => sniff(path).map(|sniffed| sniffed.ext.to_string()),
    }
}

/// A format recognised from the first bytes of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sniffed {
    /// The extension such files usually have.
    pub ext: &'static str,
    pub route: DecodeRoute,
}

/// The ASF header object (WMA, WMV).
const ASF_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
/// The EBML header (Matroska, WebM).
const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];

/// What the file's first bytes show it to be, from magic numbers that cannot
/// be mistaken for each other or for MPEG audio. MPEG audio itself (frame
/// sync or an ID3 tag) is never recognised: those bytes prove nothing.
pub(crate) fn sniff(path: &Path) -> Option<Sniffed> {
    let mut file = File::open(path).ok()?;
    let mut head = [0u8; 16];
    let mut len = 0;
    while len < head.len() {
        match file.read(&mut head[len..]) {
            Ok(0) => break,
            Ok(n) => len += n,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    sniff_bytes(&head[..len])
}

fn sniff_bytes(head: &[u8]) -> Option<Sniffed> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    let (ext, route) = if at(0, b"RIFF") && at(8, b"WAVE") {
        ("wav", DecodeRoute::Symphonia)
    } else if (at(0, b"RF64") || at(0, b"BW64")) && at(8, b"WAVE") {
        ("rf64", DecodeRoute::Ffmpeg)
    } else if at(0, b"RIFF") && at(8, b"AVI ") {
        ("avi", DecodeRoute::Ffmpeg)
    } else if at(0, b"FORM") && (at(8, b"AIFF") || at(8, b"AIFC")) {
        ("aiff", DecodeRoute::Symphonia)
    } else if at(0, b"fLaC") {
        ("flac", DecodeRoute::Symphonia)
    } else if at(0, b"OggS") {
        ("ogg", DecodeRoute::Symphonia)
    } else if at(4, b"ftyp") {
        ("mp4", DecodeRoute::Symphonia)
    } else if at(0, &EBML_MAGIC) {
        ("mkv", DecodeRoute::Ffmpeg)
    } else if at(0, &ASF_GUID) {
        ("asf", DecodeRoute::Ffmpeg)
    } else if at(0, b"caff") {
        ("caf", DecodeRoute::Ffmpeg)
    } else if at(0, b"#!AMR") {
        ("amr", DecodeRoute::Ffmpeg)
    } else {
        return None;
    };
    Some(Sniffed { ext, route })
}

/// Calls ffmpeg::find_ffmpeg(explicit). Blocking (may spawn `ffmpeg -version`).
pub fn supported_formats(explicit_ffmpeg: Option<&Path>) -> SupportedAudioFormats {
    let ffmpeg = ffmpeg::find_ffmpeg(explicit_ffmpeg);
    let owned = |list: &[&str]| list.iter().map(|ext| (*ext).to_string()).collect();
    SupportedAudioFormats {
        native: owned(SYMPHONIA_EXTENSIONS),
        ffmpeg: owned(FFMPEG_EXTENSIONS),
        ffmpeg_available: ffmpeg.is_some(),
        ffmpeg_path: ffmpeg.map(|path| path.to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_every_listed_extension_in_any_case() {
        for (list, route) in [
            (SYMPHONIA_EXTENSIONS, DecodeRoute::Symphonia),
            (FFMPEG_EXTENSIONS, DecodeRoute::Ffmpeg),
        ] {
            for ext in list {
                let mut capitalized = ext.to_string();
                capitalized[..1].make_ascii_uppercase();
                for name in [
                    format!("dir/clip.{ext}"),
                    format!("dir/clip.{}", ext.to_ascii_uppercase()),
                    format!("Clip.Name.{capitalized}"),
                ] {
                    assert_eq!(route_for_path(Path::new(&name)), Some(route), "{name}");
                }
            }
        }
    }

    #[test]
    fn extension_lists_do_not_overlap() {
        for ext in SYMPHONIA_EXTENSIONS {
            assert!(!FFMPEG_EXTENSIONS.contains(ext), "{ext} is in both lists");
        }
    }

    #[test]
    fn unknown_or_missing_extensions_are_unsupported() {
        for name in ["x.xyz", "x", "dir.wav/file", ".wav", "x.wav.txt", "x.", ""] {
            assert_eq!(route_for_path(Path::new(name)), None, "{name:?}");
        }
    }

    #[test]
    fn files_without_a_known_extension_are_routed_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, head: &[u8]| {
            let path = dir.path().join(name);
            let mut bytes = head.to_vec();
            bytes.resize(64, 0);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let wav = b"RIFF\x24\0\0\0WAVEfmt ";
        for name in ["noext", "rec.tmp", "tmp.Ab12Cd", "rec.pcm"] {
            assert_eq!(
                route_for_path(&write(name, wav)),
                Some(DecodeRoute::Symphonia),
                "{name}"
            );
            assert_eq!(
                format_extension(&dir.path().join(name)).as_deref(),
                Some("wav")
            );
        }
        let mkv = write("capture.bin", &EBML_MAGIC);
        assert_eq!(route_for_path(&mkv), Some(DecodeRoute::Ffmpeg));
        assert_eq!(format_extension(&mkv).as_deref(), Some("mkv"));

        // A known extension wins over the content.
        let opus = write("voice.opus", wav);
        assert_eq!(route_for_path(&opus), Some(DecodeRoute::Ffmpeg));
        assert_eq!(format_extension(&opus).as_deref(), Some("opus"));

        // Unrecognised content, an empty file and a missing one stay unsupported.
        for (name, head) in [("notes.xyz", &b"some text"[..]), ("blank", &b""[..])] {
            let path = dir.path().join(name);
            std::fs::write(&path, head).unwrap();
            assert_eq!(route_for_path(&path), None, "{name}");
            assert_eq!(format_extension(&path), None, "{name}");
        }
        assert_eq!(route_for_path(&dir.path().join("missing")), None);
    }

    #[test]
    fn sniffing_uses_only_unambiguous_magic_numbers() {
        let found = |ext, route| Some(Sniffed { ext, route });
        let cases: [(&[u8], Option<Sniffed>); 16] = [
            (
                b"RIFF\0\0\0\0WAVEfmt ",
                found("wav", DecodeRoute::Symphonia),
            ),
            (
                b"RF64\xff\xff\xff\xffWAVEds64",
                found("rf64", DecodeRoute::Ffmpeg),
            ),
            (
                b"BW64\xff\xff\xff\xffWAVEds64",
                found("rf64", DecodeRoute::Ffmpeg),
            ),
            (b"RIFF\0\0\0\0AVI LIST", found("avi", DecodeRoute::Ffmpeg)),
            (
                b"FORM\0\0\0\0AIFFCOMM",
                found("aiff", DecodeRoute::Symphonia),
            ),
            (
                b"FORM\0\0\0\0AIFCFVER",
                found("aiff", DecodeRoute::Symphonia),
            ),
            (b"fLaC\0\0\0\x22", found("flac", DecodeRoute::Symphonia)),
            (b"OggS\0\x02", found("ogg", DecodeRoute::Symphonia)),
            (b"\0\0\0\x20ftypM4A ", found("mp4", DecodeRoute::Symphonia)),
            (b"caff\0\x01\0\0", found("caf", DecodeRoute::Ffmpeg)),
            (b"#!AMR\n", found("amr", DecodeRoute::Ffmpeg)),
            // MPEG audio, ID3 tags, other RIFF forms and truncated headers prove nothing.
            (b"\xff\xfb\x90\x64\0\0\0\0", None),
            (b"ID3\x04\0\0\0\0\0\x10", None),
            (b"RIFF\0\0\0\0WEBPVP8 ", None),
            (b"RIFF\0\0\0\0WAV", None),
            (b"", None),
        ];
        for (head, want) in cases {
            assert_eq!(sniff_bytes(head), want, "{head:?}");
        }
        assert_eq!(
            sniff_bytes(&EBML_MAGIC).map(|sniffed| sniffed.ext),
            Some("mkv")
        );
        assert_eq!(
            sniff_bytes(&ASF_GUID).map(|sniffed| sniffed.ext),
            Some("asf")
        );
    }

    #[test]
    fn supported_formats_lists_both_groups() {
        let formats = supported_formats(Some(Path::new("/nonexistent/ffmpeg")));
        assert_eq!(formats.native.len(), SYMPHONIA_EXTENSIONS.len());
        assert_eq!(formats.ffmpeg.len(), FFMPEG_EXTENSIONS.len());
        assert!(formats.native.iter().any(|ext| ext == "m4a"));
        assert!(formats.ffmpeg.iter().any(|ext| ext == "opus"));
        assert!(!formats.ffmpeg_available);
        assert_eq!(formats.ffmpeg_path, None);
    }
}
