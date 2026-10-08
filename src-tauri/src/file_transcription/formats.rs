//! Extension routing and the supported-format list (WP1).
#![allow(dead_code, unused_variables, unused_imports)]

use crate::file_transcription::types::SupportedAudioFormats;
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

/// Case-insensitive extension routing. None = unsupported.
pub fn route_for_path(path: &Path) -> Option<DecodeRoute> {
    todo!("WP1")
}

/// Calls ffmpeg::find_ffmpeg(explicit). Blocking (may spawn `ffmpeg -version`).
pub fn supported_formats(explicit_ffmpeg: Option<&Path>) -> SupportedAudioFormats {
    todo!("WP1")
}
