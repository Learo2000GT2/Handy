//! Finding a system ffmpeg, probing durations and decoding through it (WP1).
#![allow(dead_code, unused_variables, unused_imports)]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Order: `explicit` (validated; Some(bad) → None), env HANDY_FFMPEG, PATH, then
/// macOS /opt/homebrew/bin, /usr/local/bin, /opt/local/bin; Linux /usr/bin,
/// /usr/local/bin, /snap/bin; Windows %LOCALAPPDATA%\Microsoft\WinGet\Links.
/// A path counts only if `ffmpeg -hide_banner -version` exits 0. Successful
/// auto-detection is cached in a static Mutex; misses are not cached.
pub fn find_ffmpeg(explicit: Option<&Path>) -> Option<PathBuf> {
    todo!("WP1")
}

/// `ffmpeg -hide_banner -nostdin -i <abs input>`; parse the stderr line
/// `Duration: HH:MM:SS.ff` (regex crate). None if absent/"N/A". 10 s timeout.
pub fn probe_duration_ms(ffmpeg: &Path, input: &Path) -> Option<u64> {
    todo!("WP1")
}

/// Streams 16 kHz mono f32 into `sink`. Returns samples produced.
pub fn decode_with_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[f32]) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    todo!("WP1")
}
