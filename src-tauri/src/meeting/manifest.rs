//! The meeting record (`meeting.json`), kept next to the audio in the
//! meeting's own folder. It is written before recording starts and again on
//! every change, so a meeting interrupted by a crash or power loss is found
//! and closed at the next start ([`recover`]), independently of History.

use super::wav;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::io;
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "meeting.json";
const VERSION: u32 = 1;
/// Longest title kept in file names.
const MAX_TITLE_CHARS: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum MeetingFileState {
    /// Being recorded, or interrupted if no meeting is running.
    Recording,
    /// Recording finished; not yet sent for transcription.
    Stopped,
    /// Sent to the transcription queue.
    Queued,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    /// Added by the user.
    Note,
    Paused,
    Resumed,
    /// The microphone stopped sending audio.
    MicLost,
    /// The microphone is back after `MicLost`.
    MicBack,
    /// The computer slept or stalled; `label` holds nothing, the gap length
    /// is `wall_ms` minus the previous marker's.
    Gap,
    /// The disk filled up; nothing after this marker was saved.
    DiskFull,
    /// Writing the audio failed for another reason.
    WriteFailed,
    /// Saving works again, into a follow-on audio file.
    WriteResumed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
pub struct MeetingMarker {
    /// Position in the recording, in ms of saved audio.
    pub at_ms: u64,
    /// Wall-clock time, Unix ms.
    pub wall_ms: i64,
    pub kind: MarkerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
pub struct MeetingManifest {
    pub version: u32,
    /// Folder name, also the meeting's id.
    pub id: String,
    pub title: String,
    /// Audio file name inside the meeting folder.
    pub audio_file: String,
    /// Follow-on audio files, in order, started when saving to the previous
    /// one failed.
    #[serde(default)]
    pub extra_audio_files: Vec<String>,
    /// Unix ms.
    pub started_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    pub state: MeetingFileState,
    /// Microphone name; None for the system default.
    #[serde(default)]
    pub microphone: Option<String>,
    /// When the user confirmed everyone agreed to be recorded (Unix ms).
    pub consent_confirmed_at: i64,
    #[serde(default)]
    pub label_speakers: bool,
    /// Saved audio, ms. Updated at stop and on recovery.
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub markers: Vec<MeetingMarker>,
    /// The recording ended by a crash, power loss or forced quit.
    #[serde(default)]
    pub interrupted: bool,
    /// Process that recorded it.
    #[serde(default)]
    pub pid: u32,
}

impl MeetingManifest {
    pub fn new(
        id: String,
        title: String,
        audio_file: String,
        started_at: i64,
        microphone: Option<String>,
        consent_confirmed_at: i64,
        label_speakers: bool,
    ) -> Self {
        Self {
            version: VERSION,
            id,
            title,
            audio_file,
            extra_audio_files: Vec::new(),
            started_at,
            ended_at: None,
            state: MeetingFileState::Recording,
            microphone,
            consent_confirmed_at,
            label_speakers,
            duration_ms: 0,
            markers: Vec::new(),
            interrupted: false,
            pid: std::process::id(),
        }
    }

    pub fn audio_path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.audio_file)
    }

    /// Every audio file of the meeting, in order.
    pub fn audio_paths(&self, dir: &Path) -> Vec<PathBuf> {
        std::iter::once(&self.audio_file)
            .chain(&self.extra_audio_files)
            .map(|name| dir.join(name))
            .collect()
    }

    /// Name for follow-on audio file `part` (2, 3, ...).
    pub fn part_file_name(&self, part: u32) -> String {
        let stem = self
            .audio_file
            .strip_suffix(".wav")
            .unwrap_or(&self.audio_file);
        format!("{stem} (part {part}).wav")
    }
}

pub fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(MANIFEST_FILE)
}

/// Writes the record atomically and durably.
pub fn save(dir: &Path, manifest: &MeetingManifest) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    crate::file_transcription::export::write_atomic(&manifest_path(dir), json)
}

pub fn load(dir: &Path) -> io::Result<MeetingManifest> {
    let bytes = std::fs::read(manifest_path(dir))?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The record of a folder whose `meeting.json` is missing or unreadable (an
/// interrupted save can leave only its temporary copy): that copy if it
/// reads, else a record rebuilt from the audio files, marked as recording so
/// recovery repairs them.
fn salvage(dir: &Path) -> Option<MeetingManifest> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    let temp_prefix = format!("{MANIFEST_FILE}.tmp-");
    let from_temp = entries
        .iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&temp_prefix))
        })
        .filter_map(|path| std::fs::read(path).ok())
        .find_map(|bytes| serde_json::from_slice::<MeetingManifest>(&bytes).ok());
    if from_temp.is_some() {
        return from_temp;
    }

    let mut audio: Vec<String> = entries
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "wav"))
        .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
        .collect();
    if audio.is_empty() {
        return None;
    }
    // "Name.wav" sorts before "Name (part 2).wav".
    audio.sort_by_key(|name| (name.len(), name.clone()));
    let id = dir.file_name()?.to_string_lossy().into_owned();
    let started_at =
        chrono::NaiveDateTime::parse_from_str(id.get(..19).unwrap_or(&id), "%Y-%m-%d_%H-%M-%S")
            .ok()
            .and_then(|time| time.and_local_timezone(chrono::Local).earliest())
            .map(|time| time.timestamp_millis())
            .unwrap_or(0);
    let mut manifest = MeetingManifest::new(
        id,
        String::new(),
        audio.remove(0),
        started_at,
        None,
        started_at,
        false,
    );
    manifest.extra_audio_files = audio;
    manifest.pid = 0;
    Some(manifest)
}

/// Every readable meeting under `root`, newest first.
pub fn list(root: &Path) -> Vec<(PathBuf, MeetingManifest)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut meetings: Vec<(PathBuf, MeetingManifest)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|dir| match load(&dir) {
            Ok(manifest) => Some((dir, manifest)),
            Err(error) => {
                let salvaged = salvage(&dir);
                if salvaged.is_some() || manifest_path(&dir).exists() {
                    log::warn!("Meeting record in {:?} unreadable: {}", dir, error);
                }
                salvaged.map(|manifest| (dir, manifest))
            }
        })
        .collect();
    meetings.sort_by_key(|meeting| std::cmp::Reverse(meeting.1.started_at));
    meetings
}

/// Closes a meeting whose recording was interrupted: repairs the audio
/// file's header so every saved byte is readable, and marks the record
/// stopped and interrupted. Call only when no meeting is being recorded.
/// Returns true when the record changed.
pub fn recover(dir: &Path, manifest: &mut MeetingManifest) -> io::Result<bool> {
    if manifest.state != MeetingFileState::Recording {
        return Ok(false);
    }
    let mut samples = 0;
    for audio in manifest.audio_paths(dir) {
        match wav::repair(&audio) {
            Ok(part) => samples += part,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("Could not repair {:?}: {}", audio, error),
        }
    }
    let last_audio = manifest.audio_paths(dir).pop().unwrap_or_default();
    let ended_at = std::fs::metadata(&last_audio)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as i64)
        .unwrap_or(manifest.started_at);
    manifest.duration_ms = wav::samples_to_ms(samples);
    manifest.ended_at = Some(ended_at.max(manifest.started_at));
    manifest.interrupted = true;
    manifest.state = MeetingFileState::Stopped;
    save(dir, manifest)?;
    Ok(true)
}

/// A title as it can appear in a file name on every platform.
pub fn file_safe_title(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(MAX_TITLE_CHARS).collect();
    // Windows drops trailing dots and spaces from names.
    truncated.trim_end_matches(['.', ' ']).to_string()
}

/// Audio file name for a meeting started at `stamp` ("2026-10-13 14-05").
pub fn audio_file_name(stamp: &str, title: &str) -> String {
    let title = file_safe_title(title);
    if title.is_empty() {
        format!("Meeting {stamp}.wav")
    } else {
        format!("Meeting {stamp} {title}.wav")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, started_at: i64) -> MeetingManifest {
        MeetingManifest::new(
            id.to_string(),
            "Review".to_string(),
            "Meeting.wav".to_string(),
            started_at,
            Some("USB Mic".to_string()),
            started_at,
            true,
        )
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = sample("a", 1_000);
        manifest.markers.push(MeetingMarker {
            at_ms: 61_000,
            wall_ms: 62_000,
            kind: MarkerKind::Note,
            label: Some("Job offer".to_string()),
        });
        save(dir.path(), &manifest).unwrap();
        assert_eq!(load(dir.path()).unwrap(), manifest);
    }

    #[test]
    fn list_is_newest_first_and_skips_folders_without_a_record() {
        let root = tempfile::tempdir().unwrap();
        for (id, at) in [("old", 1), ("new", 3), ("mid", 2)] {
            let dir = root.path().join(id);
            std::fs::create_dir(&dir).unwrap();
            save(&dir, &sample(id, at)).unwrap();
        }
        std::fs::create_dir(root.path().join("empty")).unwrap();
        let broken = root.path().join("broken");
        std::fs::create_dir(&broken).unwrap();
        std::fs::write(manifest_path(&broken), b"{").unwrap();

        let ids: Vec<String> = list(root.path()).into_iter().map(|(_, m)| m.id).collect();
        assert_eq!(ids, ["new", "mid", "old"]);
    }

    #[test]
    fn recover_closes_an_interrupted_recording() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = sample("a", 1_000);
        let mut audio = wav::DurableWav::create(&manifest.audio_path(dir.path())).unwrap();
        audio.write(&vec![0.25; 32_000]).unwrap();
        audio.checkpoint().unwrap();
        std::mem::forget(audio);
        save(dir.path(), &manifest).unwrap();

        assert!(recover(dir.path(), &mut manifest).unwrap());
        assert_eq!(manifest.state, MeetingFileState::Stopped);
        assert!(manifest.interrupted);
        assert_eq!(manifest.duration_ms, 2_000);
        assert!(manifest.ended_at.unwrap() >= 1_000);
        assert_eq!(load(dir.path()).unwrap(), manifest);

        // Already closed: nothing to do.
        assert!(!recover(dir.path(), &mut manifest).unwrap());
    }

    #[test]
    fn recover_handles_a_missing_audio_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = sample("a", 1_000);
        save(dir.path(), &manifest).unwrap();
        assert!(recover(dir.path(), &mut manifest).unwrap());
        assert_eq!(manifest.duration_ms, 0);
        assert_eq!(manifest.ended_at, Some(1_000));
    }

    #[test]
    fn recover_adds_up_follow_on_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = sample("a", 1_000);
        manifest.extra_audio_files = vec![manifest.part_file_name(2)];
        assert_eq!(manifest.extra_audio_files[0], "Meeting (part 2).wav");
        for path in manifest.audio_paths(dir.path()) {
            let mut audio = wav::DurableWav::create(&path).unwrap();
            audio.write(&vec![0.25; 16_000]).unwrap();
            audio.checkpoint().unwrap();
            std::mem::forget(audio);
        }
        assert!(recover(dir.path(), &mut manifest).unwrap());
        assert_eq!(manifest.duration_ms, 2_000);
    }

    #[test]
    fn a_folder_with_audio_but_no_record_is_listed_for_recovery() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("2026-10-13_14-05-22");
        std::fs::create_dir(&dir).unwrap();
        for name in [
            "Meeting 2026-10-13 14-05.wav",
            "Meeting 2026-10-13 14-05 (part 2).wav",
        ] {
            let mut audio = wav::DurableWav::create(&dir.join(name)).unwrap();
            audio.write(&vec![0.25; 16_000]).unwrap();
            audio.checkpoint().unwrap();
            std::mem::forget(audio);
        }

        let listed = list(root.path());
        assert_eq!(listed.len(), 1);
        let (dir, mut manifest) = listed.into_iter().next().unwrap();
        assert_eq!(manifest.id, "2026-10-13_14-05-22");
        assert_eq!(manifest.audio_file, "Meeting 2026-10-13 14-05.wav");
        assert_eq!(
            manifest.extra_audio_files,
            ["Meeting 2026-10-13 14-05 (part 2).wav"]
        );
        assert!(manifest.started_at > 0);
        assert_eq!(manifest.state, MeetingFileState::Recording);
        assert!(recover(&dir, &mut manifest).unwrap());
        assert_eq!(manifest.duration_ms, 2_000);
        // Saved now, so it loads normally from here on.
        assert_eq!(load(&dir).unwrap(), manifest);
    }

    #[test]
    fn a_left_over_temporary_record_is_used() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a");
        std::fs::create_dir(&dir).unwrap();
        let manifest = sample("a", 5);
        std::fs::write(
            dir.join(format!("{MANIFEST_FILE}.tmp-123")),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(list(root.path()), vec![(dir, manifest)]);
    }

    #[test]
    fn titles_are_safe_in_file_names() {
        assert_eq!(
            file_safe_title("  Ms. Smith: plan / review?  "),
            "Ms. Smith plan review"
        );
        assert_eq!(file_safe_title("Notes..."), "Notes");
        assert_eq!(file_safe_title("a\tb\nc"), "a b c");
        assert_eq!(file_safe_title(&"x".repeat(80)).len(), 60);
        assert_eq!(
            audio_file_name("2026-10-13 14-05", ""),
            "Meeting 2026-10-13 14-05.wav"
        );
        assert_eq!(
            audio_file_name("2026-10-13 14-05", "Mr. Lee intake"),
            "Meeting 2026-10-13 14-05 Mr. Lee intake.wav"
        );
    }
}
