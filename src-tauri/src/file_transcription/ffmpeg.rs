//! A system ffmpeg: finding it, probing an input, and decoding through it.
//!
//! ffmpeg is never bundled. It decodes the formats symphonia cannot (Opus,
//! WebM, WMA, AMR, DSS, ALAC, ...) and is the fallback when symphonia fails on
//! a file before producing any audio. It always runs as a separate process
//! with argv entries (no shell), local files only, and is killed and reaped on
//! every exit path.

use anyhow::{anyhow, bail, Context};
use log::{debug, warn};
use once_cell::sync::Lazy;
use regex::Regex;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Environment variable naming the ffmpeg binary to use.
pub const FFMPEG_ENV: &str = "HANDY_FFMPEG";

const EXE_NAME: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};
/// `ffmpeg -version` answers in milliseconds; anything slower is not usable.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL: Duration = Duration::from_millis(10);
/// Bytes of decoded PCM read from ffmpeg's stdout per read.
const READ_BLOCK_BYTES: usize = 64 * 1024;
/// How much of ffmpeg's stderr is kept for error messages.
const STDERR_TAIL_BYTES: usize = 8 * 1024;
/// Upper bound on what the probe keeps from `ffmpeg -i` (lines that matter).
const PROBE_KEEP_BYTES: usize = 64 * 1024;
const PROBE_MAX_LINE: u64 = 4 * 1024;

/// Successful auto-detection (env var or search); misses are not cached so
/// installing ffmpeg needs no restart.
static DETECTED: Mutex<Option<PathBuf>> = Mutex::new(None);

static DURATION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"Duration:\s*(\d+):(\d{2}):(\d{2})(?:\.(\d+))?").expect("valid duration regex")
});
static AUDIO_STREAM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*Stream #\d+:\d+.*?: Audio:").expect("valid stream regex"));

/// Order: `explicit` (validated; Some(bad) → None), env HANDY_FFMPEG, PATH, then
/// macOS /opt/homebrew/bin, /usr/local/bin, /opt/local/bin; Linux /usr/bin,
/// /usr/local/bin, /snap/bin; Windows %LOCALAPPDATA%\Microsoft\WinGet\Links.
/// A path counts only if `ffmpeg -hide_banner -version` exits 0. Successful
/// auto-detection is cached in a static Mutex; misses are not cached.
pub fn find_ffmpeg(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        if ffmpeg_works(path) {
            return Some(path.to_path_buf());
        }
        warn!("ffmpeg at {} does not run; not using it", path.display());
        return None;
    }

    // Held across the search so concurrent callers do not each spawn
    // `ffmpeg -version` for every candidate.
    let mut detected = DETECTED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(path) = detected.as_ref() {
        // A bare command name from HANDY_FFMPEG is resolved through PATH.
        if path.is_file() || !path.is_absolute() {
            return Some(path.clone());
        }
        // Uninstalled or moved since it was found.
        *detected = None;
    }

    if let Some(env_path) = std::env::var_os(FFMPEG_ENV).filter(|value| !value.is_empty()) {
        let env_path = PathBuf::from(env_path);
        if ffmpeg_works(&env_path) {
            debug!("Using ffmpeg from {FFMPEG_ENV}: {}", env_path.display());
            *detected = Some(env_path.clone());
            return Some(env_path);
        }
        warn!(
            "{FFMPEG_ENV}={} does not run; searching PATH instead",
            env_path.display()
        );
    }

    let found = search_dirs()
        .into_iter()
        .map(|dir| dir.join(EXE_NAME))
        .find(|candidate| candidate.is_file() && ffmpeg_works(candidate))?;
    debug!("Found ffmpeg at {}", found.display());
    *detected = Some(found.clone());
    Some(found)
}

/// PATH first, then folders GUI apps often miss (macOS apps do not inherit
/// the shell PATH). Relative PATH entries are skipped so that an `ffmpeg`
/// in the current directory is never picked up.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].map(PathBuf::from));
    #[cfg(target_os = "linux")]
    dirs.extend(["/usr/bin", "/usr/local/bin", "/snap/bin"].map(PathBuf::from));
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join(r"Microsoft\WinGet\Links"));
    }
    dirs
}

fn ffmpeg_works(path: &Path) -> bool {
    let mut cmd = command(path);
    cmd.args(["-hide_banner", "-version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(child) = cmd.spawn() else {
        return false;
    };
    let mut child = ChildGuard(child);
    matches!(
        wait_timeout(&mut child.0, VERSION_TIMEOUT),
        Ok(Some(status)) if status.success()
    )
}

/// What `ffmpeg -i` reported about an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InputInfo {
    /// ffmpeg opened and understood the input.
    Opened {
        duration_ms: Option<u64>,
        has_audio: bool,
    },
    /// ffmpeg could not open or understand it (its last message).
    Unreadable(String),
}

/// Runs `ffmpeg -i` on `input`. None when ffmpeg could not be run or did not
/// answer within 10 s, so nothing is known about the input.
pub(crate) fn probe_input(ffmpeg: &Path, input: &Path) -> Option<InputInfo> {
    let input = std::path::absolute(input).ok()?;
    let mut cmd = command(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-protocol_whitelist",
        "file",
        "-i",
    ])
    .arg(&input)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => ChildGuard(child),
        Err(err) => {
            warn!("Could not start {}: {err}", ffmpeg.display());
            return None;
        }
    };
    let stderr = child.0.stderr.take()?;
    let reader = std::thread::Builder::new()
        .name("ffmpeg-probe".into())
        .spawn(move || keep_probe_lines(stderr))
        .ok()?;

    let status = wait_timeout(&mut child.0, PROBE_TIMEOUT);
    // Kills ffmpeg if it is still running, which also ends the reader.
    drop(child);
    let text = reader.join().ok()?;
    match status {
        Ok(Some(_)) => Some(parse_input_info(&text)),
        Ok(None) => {
            warn!("ffmpeg took too long to probe {}", input.display());
            None
        }
        Err(err) => {
            warn!("Waiting for ffmpeg failed: {err}");
            None
        }
    }
}

/// `ffmpeg -hide_banner -nostdin -i <abs input>`; parse the stderr line
/// `Duration: HH:MM:SS.ff` (regex crate). None if absent/"N/A". 10 s timeout.
pub fn probe_duration_ms(ffmpeg: &Path, input: &Path) -> Option<u64> {
    match probe_input(ffmpeg, input)? {
        InputInfo::Opened { duration_ms, .. } => duration_ms,
        InputInfo::Unreadable(_) => None,
    }
}

/// Keeps the lines of `ffmpeg -i` output that `parse_input_info` needs (input
/// header, duration, streams) plus the last line, with bounded memory even
/// when ffmpeg prints thousands of warnings.
fn keep_probe_lines(stderr: impl Read) -> String {
    let mut reader = BufReader::new(stderr);
    let mut kept = String::new();
    let mut last = String::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(PROBE_MAX_LINE)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end();
                if text.trim().is_empty() {
                    continue;
                }
                let relevant = text.starts_with("Input #")
                    || text.contains("Duration:")
                    || text.trim_start().starts_with("Stream #");
                if relevant && kept.len() + text.len() < PROBE_KEEP_BYTES {
                    kept.push_str(text);
                    kept.push('\n');
                }
                last.clear();
                last.push_str(text);
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    kept.push_str(&last);
    kept
}

fn parse_input_info(stderr: &str) -> InputInfo {
    if !stderr.lines().any(|line| line.starts_with("Input #")) {
        let reason = stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("ffmpeg could not read the file");
        return InputInfo::Unreadable(reason.to_string());
    }
    InputInfo::Opened {
        duration_ms: parse_duration_ms(stderr),
        has_audio: stderr.lines().any(|line| AUDIO_STREAM_RE.is_match(line)),
    }
}

/// The first `Duration: HH:MM:SS.ff` (input #0). None for `Duration: N/A`.
fn parse_duration_ms(stderr: &str) -> Option<u64> {
    let caps = DURATION_RE.captures(stderr)?;
    let field = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<u64>().ok());
    let (hours, minutes, seconds) = (field(1)?, field(2)?, field(3)?);
    let millis = caps.get(4).map_or(0, |frac| {
        let digits: String = frac.as_str().chars().take(3).collect();
        let value = digits.parse::<u64>().unwrap_or(0);
        value * 10u64.pow(3 - digits.len() as u32)
    });
    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + millis)
}

/// Fills a delayed start (audio that begins after the video) and timestamp
/// gaps (lost packets) with silence, since raw PCM output has no timestamps
/// to keep them. Like symphonia's route in decode.rs, it leaves offsets up
/// to 100 ms alone (min_comp), which keeps the length of clean files whose
/// codec delay shows in the timestamps (WMA, Opus in WebM).
const TIMELINE_FILTER: &str = "aresample=async=1:min_comp=0.1:first_pts=0";

/// Streams 16 kHz mono f32 into `sink`. Returns samples produced.
pub fn decode_with_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[f32]) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    let input = std::path::absolute(input)
        .with_context(|| format!("invalid input path {}", input.display()))?;
    let mut cmd = command(ffmpeg);
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        // No network or other protocols reachable from a crafted input.
        "-protocol_whitelist",
        "file",
        "-i",
    ])
    .arg(&input)
    .args([
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-af",
        TIMELINE_FILTER,
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_f32le",
        "-f",
        "f32le",
        "pipe:1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = ChildGuard(
        cmd.spawn()
            .with_context(|| format!("could not start {}", ffmpeg.display()))?,
    );
    let mut stdout = child
        .0
        .stdout
        .take()
        .context("ffmpeg stdout is not piped")?;
    let stderr = child
        .0
        .stderr
        .take()
        .context("ffmpeg stderr is not piped")?;
    // Drained concurrently so ffmpeg never blocks on a full stderr pipe.
    let stderr_tail = std::thread::Builder::new()
        .name("ffmpeg-stderr".into())
        .spawn(move || read_tail(stderr, STDERR_TAIL_BYTES))
        .context("could not start the ffmpeg stderr reader")?;

    let mut bytes = vec![0u8; READ_BLOCK_BYTES];
    // Bytes of a sample split across reads, kept at the front of `bytes`.
    let mut carry = 0usize;
    let mut samples: Vec<f32> = Vec::with_capacity(READ_BLOCK_BYTES / 4);
    let mut total: u64 = 0;
    let streamed: anyhow::Result<()> = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(anyhow!("cancelled"));
        }
        let n = match stdout.read(&mut bytes[carry..]) {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => break Err(anyhow::Error::new(err).context("reading ffmpeg output")),
        };
        let filled = carry + n;
        let whole = filled - filled % 4;
        samples.clear();
        samples.extend(bytes[..whole].chunks_exact(4).map(|b| {
            let sample = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            if sample.is_finite() {
                sample
            } else {
                0.0
            }
        }));
        bytes.copy_within(whole..filled, 0);
        carry = filled - whole;
        if samples.is_empty() {
            continue;
        }
        if let Err(err) = sink(&samples) {
            break Err(err);
        }
        total += samples.len() as u64;
    };

    drop(stdout);
    if streamed.is_err() {
        // Cancelled, or the sink/pipe failed: ffmpeg may still be writing.
        let _ = child.0.kill();
    }
    let status = child.0.wait().context("waiting for ffmpeg");
    let tail = stderr_tail.join().unwrap_or_default();
    streamed?;
    let status = status?;
    if !status.success() {
        bail!("ffmpeg failed ({status}): {}", summarize(&tail));
    }
    if total == 0 {
        let detail = summarize(&tail);
        if detail.is_empty() {
            bail!("ffmpeg produced no audio");
        }
        bail!("ffmpeg produced no audio: {detail}");
    }
    Ok(total)
}

/// The last few non-empty lines of ffmpeg's stderr, on one line.
fn summarize(stderr: &str) -> String {
    const LINES: usize = 4;
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(LINES)..].join(" / ")
}

/// Reads to the end, keeping the last `limit` bytes.
fn read_tail(mut reader: impl Read, limit: usize) -> String {
    let mut tail = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > limit {
                    tail.drain(..tail.len() - limit);
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

fn command(program: &Path) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Waits up to `timeout` for the child to exit. Ok(None) means it is still running.
fn wait_timeout(child: &mut Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(WAIT_POLL);
    }
}

/// Kills and reaps the child on every exit path (errors, cancel, timeouts and
/// panics in the sink), so no ffmpeg is left running or as a zombie.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.0.try_wait() {
            return;
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The ffmpeg to run fixture tests with, or None (after saying so) when
    /// it is not installed, as on CI.
    pub(crate) fn ffmpeg_or_skip(test: &str) -> Option<PathBuf> {
        let found = find_ffmpeg(None);
        if found.is_none() {
            eprintln!("skipping {test}: ffmpeg not found");
        }
        found
    }

    /// Runs ffmpeg with `args` to make a test fixture. False (after saying
    /// why) when this ffmpeg build cannot, for example for lack of an encoder.
    pub(crate) fn make_fixture(ffmpeg: &Path, args: &[&str]) -> bool {
        let output = command(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(args)
            .output()
            .expect("run ffmpeg");
        if !output.status.success() {
            eprintln!(
                "skipping a fixture: ffmpeg {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output.status.success()
    }

    const MP4_WITH_VIDEO: &str = "\
Input #0, mov,mp4,m4a,3gp,3g2,mj2, from '/tmp/clip.mp4':
  Metadata:
    major_brand     : isom
    minor_version   : 512
    compatible_brands: isomiso2mp41
    encoder         : Lavf60.16.100
  Duration: 00:00:03.50, start: 0.000000, bitrate: 165 kb/s
  Stream #0:0[0x1](und): Video: mpeg4 (Simple Profile) (mp4v / 0x7634706D), yuv420p, 64x48 [SAR 1:1 DAR 4:3], 30 kb/s, 10 fps, 10 tbr, 10240 tbn (default)
    Metadata:
      handler_name    : VideoHandler
      vendor_id       : [0][0][0][0]
      encoder         : Lavc60.31.102 mpeg4
  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, stereo, fltp, 128 kb/s (default)
    Metadata:
      handler_name    : SoundHandler
      vendor_id       : [0][0][0][0]
At least one output file must be specified
";

    const JUNK: &str = "\
[in#0 @ 0x55877d026e00] Error opening input: Invalid data found when processing input
Error opening input file /tmp/junk.wav.
Error opening input files: Invalid data found when processing input
";

    #[test]
    fn parses_captured_probe_output() {
        assert_eq!(parse_duration_ms(MP4_WITH_VIDEO), Some(3_500));
        assert_eq!(
            parse_input_info(MP4_WITH_VIDEO),
            InputInfo::Opened {
                duration_ms: Some(3_500),
                has_audio: true
            }
        );
        // The filtered form the probe keeps parses the same.
        let kept = keep_probe_lines(MP4_WITH_VIDEO.as_bytes());
        assert!(!kept.contains("handler_name"));
        assert_eq!(parse_input_info(&kept), parse_input_info(MP4_WITH_VIDEO));
    }

    #[test]
    fn parses_durations_and_missing_audio() {
        assert_eq!(
            parse_duration_ms("  Duration: 01:02:03.4, start"),
            Some(3_723_400)
        );
        assert_eq!(parse_duration_ms("  Duration: 00:00:00.123456"), Some(123));
        assert_eq!(
            parse_duration_ms("  Duration: 10:00:00, start"),
            Some(36_000_000)
        );
        assert_eq!(parse_duration_ms("  Duration: N/A, bitrate: N/A"), None);
        assert_eq!(parse_duration_ms(""), None);

        let video_only = MP4_WITH_VIDEO
            .lines()
            .filter(|line| !line.contains("Audio:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            parse_input_info(&video_only),
            InputInfo::Opened {
                duration_ms: Some(3_500),
                has_audio: false
            }
        );
    }

    #[test]
    fn unreadable_input_reports_ffmpegs_last_line() {
        assert_eq!(
            parse_input_info(JUNK),
            InputInfo::Unreadable(
                "Error opening input files: Invalid data found when processing input".into()
            )
        );
        assert!(matches!(parse_input_info(""), InputInfo::Unreadable(_)));
    }

    #[test]
    fn summarize_keeps_the_last_lines() {
        assert_eq!(summarize("a\n\n b \nc\nd\ne\n"), "b / c / d / e");
        assert_eq!(summarize(""), "");
    }

    #[test]
    fn read_tail_keeps_the_end() {
        let text = "x".repeat(10_000) + "end";
        let tail = read_tail(text.as_bytes(), 100);
        assert_eq!(tail.len(), 100);
        assert!(tail.ends_with("end"));
    }

    #[test]
    fn explicit_path_that_does_not_run_is_rejected() {
        assert_eq!(find_ffmpeg(Some(Path::new("/nonexistent/ffmpeg"))), None);
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join(EXE_NAME);
        std::fs::write(&fake, b"not a program").unwrap();
        assert_eq!(find_ffmpeg(Some(&fake)), None);
    }

    #[test]
    fn finds_and_caches_a_working_ffmpeg() {
        let Some(found) = ffmpeg_or_skip("finds_and_caches_a_working_ffmpeg") else {
            return;
        };
        assert!(ffmpeg_works(&found));
        assert_eq!(find_ffmpeg(None), Some(found.clone()));
        assert_eq!(find_ffmpeg(Some(&found)), Some(found));
    }

    #[test]
    fn decodes_and_probes_with_ffmpeg() {
        let Some(ffmpeg) = ffmpeg_or_skip("decodes_and_probes_with_ffmpeg") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("tone.flac");
        if !make_fixture(
            &ffmpeg,
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=2.5",
                clip.to_str().unwrap(),
            ],
        ) {
            return;
        }
        assert_eq!(probe_duration_ms(&ffmpeg, &clip), Some(2_500));

        let mut got = Vec::new();
        let cancel = AtomicBool::new(false);
        let total = decode_with_ffmpeg(&ffmpeg, &clip, &cancel, &mut |block| {
            got.extend_from_slice(block);
            Ok(())
        })
        .unwrap();
        assert_eq!(total, got.len() as u64);
        assert!((got.len() as i64 - 40_000).abs() <= 1, "{}", got.len());
        let peak = got.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05, "peak {peak}");
    }

    /// Peak level of `samples[from_ms..to_ms]`.
    fn peak_between(samples: &[f32], from_ms: usize, to_ms: usize) -> f32 {
        samples[from_ms * 16..to_ms * 16]
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn delayed_starts_and_gaps_become_silence() {
        let Some(ffmpeg) = ffmpeg_or_skip("delayed_starts_and_gaps_become_silence") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let tone = "sine=frequency=440:sample_rate=48000:duration=2";
        // Video from 0 s, a 2 s tone from 1.5 s.
        let delayed = dir.path().join("delayed.mkv");
        // 2 s of tone, a 3 s hole in the timestamps, 2 s of tone.
        let gapped = dir.path().join("gapped.mka");
        let gap_tone = "sine=frequency=440:sample_rate=48000:duration=4";
        let made = make_fixture(
            &ffmpeg,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=4:size=64x48:rate=10",
                "-itsoffset",
                "1.5",
                "-f",
                "lavfi",
                "-i",
                tone,
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "mpeg4",
                "-c:a",
                "flac",
                delayed.to_str().unwrap(),
            ],
        ) && make_fixture(
            &ffmpeg,
            &[
                "-f",
                "lavfi",
                "-i",
                gap_tone,
                "-af",
                "asetpts='PTS+gte(T,2)*3/TB'",
                "-c:a",
                "flac",
                gapped.to_str().unwrap(),
            ],
        );
        if !made {
            return;
        }

        let decode = |path: &Path| {
            let mut got = Vec::new();
            decode_with_ffmpeg(&ffmpeg, path, &AtomicBool::new(false), &mut |block| {
                got.extend_from_slice(block);
                Ok(())
            })
            .unwrap();
            got
        };
        let got = decode(&delayed);
        assert!((got.len() as i64 - 56_000).abs() <= 160, "{}", got.len());
        assert!(peak_between(&got, 0, 1_400) < 1e-3);
        assert!(peak_between(&got, 1_600, 3_400) > 0.05);

        let got = decode(&gapped);
        assert!((got.len() as i64 - 112_000).abs() <= 160, "{}", got.len());
        assert!(peak_between(&got, 100, 1_900) > 0.05);
        assert!(peak_between(&got, 2_100, 4_900) < 1e-3);
        assert!(peak_between(&got, 5_100, 6_900) > 0.05);
    }

    #[test]
    fn ffmpeg_errors_carry_its_message() {
        let Some(ffmpeg) = ffmpeg_or_skip("ffmpeg_errors_carry_its_message") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.opus");
        std::fs::write(&junk, "this is not audio at all").unwrap();
        assert_eq!(probe_duration_ms(&ffmpeg, &junk), None);
        assert!(matches!(
            probe_input(&ffmpeg, &junk),
            Some(InputInfo::Unreadable(_))
        ));
        let err = decode_with_ffmpeg(&ffmpeg, &junk, &AtomicBool::new(false), &mut |_| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("ffmpeg failed"), "{err}");
        assert!(err.contains("Invalid data"), "{err}");
    }

    #[test]
    fn sink_errors_and_cancel_stop_ffmpeg() {
        let Some(ffmpeg) = ffmpeg_or_skip("sink_errors_and_cancel_stop_ffmpeg") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // Long enough that ffmpeg is still writing when the sink fails.
        let clip = dir.path().join("long.wav");
        if !make_fixture(
            &ffmpeg,
            &[
                "-f",
                "lavfi",
                "-i",
                "anoisesrc=duration=120:sample_rate=16000",
                clip.to_str().unwrap(),
            ],
        ) {
            return;
        }

        let mut calls = 0;
        let err = decode_with_ffmpeg(&ffmpeg, &clip, &AtomicBool::new(false), &mut |_| {
            calls += 1;
            if calls == 3 {
                bail!("sink is full");
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "sink is full");

        let cancel = AtomicBool::new(false);
        let mut seen = 0usize;
        let err = decode_with_ffmpeg(&ffmpeg, &clip, &cancel, &mut |block| {
            seen += block.len();
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
        assert!(seen < 120 * 16_000);
    }
}
