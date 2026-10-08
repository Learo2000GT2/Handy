//! Which decoder handles which file extension, and the list shown to users.
//!
//! Routing is by extension only: symphonia 0.5.5 ignores the extension hint
//! and probes for 2-byte MPEG sync markers, so sniffing the content would
//! misidentify some of the ffmpeg-only formats as MP3.

use crate::file_transcription::{ffmpeg, types::SupportedAudioFormats};
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

/// Case-insensitive extension routing. None = unsupported.
pub fn route_for_path(path: &Path) -> Option<DecodeRoute> {
    let ext = lowercase_extension(path)?;
    if SYMPHONIA_EXTENSIONS.contains(&ext.as_str()) {
        Some(DecodeRoute::Symphonia)
    } else if FFMPEG_EXTENSIONS.contains(&ext.as_str()) {
        Some(DecodeRoute::Ffmpeg)
    } else {
        None
    }
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
