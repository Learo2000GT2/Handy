//! Live meetings: records an in-person meeting from one microphone to a WAV
//! file in its own folder, shows a live transcript preview while it runs,
//! and sends the recording to the transcription queue (with speaker labels)
//! when it stops.
//!
//! The audio file is the product. It is written as it is recorded and
//! forced to disk every [`CHECKPOINT`], next to a record file
//! ([`manifest`]) written before recording starts, so a crash or power loss
//! loses at most a few seconds and the meeting is closed at the next start.
//! The meeting has its own recorder, separate from dictation: Escape, the
//! tray's Cancel and dictation shortcuts never touch it, and dictation is
//! refused while a meeting runs.
//!
//! Threads per meeting:
//! - the recorder's own capture threads, which hand 16 kHz frames to a
//!   bounded queue without ever waiting;
//! - the writer: frames to the WAV file, levels, checkpoints, and the live
//!   transcript's queue (again without waiting);
//! - the supervisor: keeps the computer awake, notices a microphone that
//!   stopped delivering audio (unplugged, or the computer slept) and reopens
//!   it, records gaps, and sends the status to the window;
//! - the live transcript ([`live`]).

pub mod level;
pub mod live;
pub mod manifest;
pub mod power;
pub mod wav;

use crate::audio_toolkit::{list_input_devices, AudioRecorder, VadPolicy};
use crate::engine_supervisor::WorkToken;
use crate::file_transcription::manager::FileTranscriptionManager;
use crate::file_transcription::types::{FileTranscriptionOptions, TranscriptSegment};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;
use level::{LevelMeter, MicCheckStats};
use live::{LiveEvent, LiveFrame, LiveStatus};
use manifest::{MarkerKind, MeetingFileState, MeetingManifest, MeetingMarker};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

/// How often the audio file is made durable.
const CHECKPOINT: Duration = Duration::from_secs(2);
/// Frames (30 ms each) the writer's queue holds: one minute.
const WRITER_QUEUE_FRAMES: usize = 2_000;
/// Frames the live transcript's queue holds: 30 seconds.
const LIVE_QUEUE_FRAMES: usize = 1_000;
/// How often the supervisor checks health and sends the status.
const TICK: Duration = Duration::from_millis(250);
/// No audio for this long means the microphone stopped.
const MIC_STALL: Duration = Duration::from_secs(2);
const MIC_RETRY: Duration = Duration::from_secs(3);
/// A wall-clock jump this large between ticks means the computer slept.
const SLEEP_GAP: Duration = Duration::from_secs(10);
/// Exact silence for this long: muted, or access blocked.
const NO_SIGNAL_SAMPLES: u64 = 3 * 16_000;
/// No sound above [`SOUND_DB`] for this long: too quiet to be a meeting.
const QUIET_SAMPLES: u64 = 45 * 16_000;
const SOUND_DB: f32 = -50.0;
/// Free space needed to start (about four hours of audio).
pub const MIN_FREE_BYTES_TO_START: u64 = 500 * 1024 * 1024;
/// Below this the status warns that the disk is nearly full.
const LOW_FREE_BYTES: u64 = 200 * 1024 * 1024;
/// How long the microphone may take to deliver its first audio.
const MIC_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Microphone check length.
pub const MIC_CHECK_MS: u64 = 10_000;
/// How often a new audio file is tried while saving fails.
const WRITE_RETRY: Duration = Duration::from_secs(5);
/// Audio kept in memory while saving fails: five minutes.
const MAX_BACKLOG_SAMPLES: usize = 5 * 60 * 16_000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static QUIT_CONFIRMED: AtomicBool = AtomicBool::new(false);
/// When quitting was last held back (Unix ms), for [`hold_quit`].
static QUIT_HELD_AT: AtomicI64 = AtomicI64::new(0);
/// A second quit within this time goes through: the window may be unable to
/// show the confirmation, and the exit handler saves the meeting anyway.
const SECOND_QUIT_MS: i64 = 15_000;

/// True while a meeting is being recorded.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Lets the next quit through even though a meeting is recording (the user
/// confirmed it, after the meeting was stopped).
pub fn confirm_quit() {
    QUIT_CONFIRMED.store(true, Ordering::SeqCst);
}

pub fn quit_confirmed() -> bool {
    QUIT_CONFIRMED.load(Ordering::SeqCst)
}

/// Whether to hold back a quit request: true while a meeting records,
/// unless the user confirmed, or asked to quit again soon after being asked
/// to confirm. Exit then saves the meeting ([`MeetingManager::shutdown`]).
pub fn hold_quit() -> bool {
    if !is_active() || quit_confirmed() {
        return false;
    }
    let now = now_ms();
    let held_at = QUIT_HELD_AT.swap(now, Ordering::SeqCst);
    now - held_at > SECOND_QUIT_MS
}

pub fn meetings_dir(app: &AppHandle) -> Result<PathBuf, String> {
    crate::portable::app_data_dir(app)
        .map(|dir| dir.join("meetings"))
        .map_err(|e| format!("Failed to find the app data folder: {}", e))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RecordingPhase {
    Recording,
    Paused,
    /// Saving the audio after Stop.
    Stopping,
    /// Writing failed (usually a full disk). The audio up to then is saved;
    /// press Stop to finish.
    Failed,
}

/// What the microphone is delivering, worst first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SignalState {
    /// No audio arrives; reconnecting.
    Lost,
    /// Exact silence: muted, or microphone access blocked.
    NoSignal,
    /// Nothing louder than a whisper for a while.
    Quiet,
    Ok,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingStatus {
    pub id: String,
    pub title: String,
    pub folder: String,
    pub audio_path: String,
    /// Unix ms.
    pub started_at: i64,
    pub microphone: Option<String>,
    pub phase: RecordingPhase,
    pub signal: SignalState,
    /// Audio saved so far, ms.
    pub recorded_ms: u64,
    pub rms_db: f32,
    pub peak_db: f32,
    pub markers: Vec<MeetingMarker>,
    pub live: LiveStatus,
    /// Audio the live preview skipped because it fell behind, ms.
    pub live_skipped_ms: u64,
    /// Audio lost before it reached the file, ms (should stay 0).
    pub dropped_ms: u64,
    /// Free space on the meetings disk, if known.
    pub free_bytes: Option<u64>,
    pub low_disk: bool,
    pub write_error: Option<String>,
}

/// A meeting folder, for the list of past meetings.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingSummary {
    pub manifest: MeetingManifest,
    pub folder: String,
    pub audio_path: String,
    pub audio_exists: bool,
    /// History entry with its transcript, once transcribed.
    pub history_id: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct StartMeetingOptions {
    pub title: String,
    /// The user confirmed everyone present agreed to be recorded.
    pub consent_confirmed: bool,
    pub label_speakers: bool,
    pub live_transcript: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MicCheckResult {
    pub stats: MicCheckStats,
    /// What Handy transcribed from the check ("" if nothing, None if the
    /// model could not run).
    pub heard: Option<String>,
    pub microphone: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MeetingEvent {
    /// The running meeting's state, about four times a second, or None when
    /// no meeting runs.
    Status { status: Option<MeetingStatus> },
    /// New live transcript segments of the running meeting.
    Segments {
        meeting_id: String,
        segments: Vec<TranscriptSegment>,
    },
    /// Microphone check level, about four times a second.
    CheckLevel {
        rms_db: f32,
        peak_db: f32,
        elapsed_ms: u64,
    },
    /// A meeting finished recording.
    Stopped { meeting: MeetingSummary },
    /// Quitting was held back because a meeting is recording.
    QuitRequested,
}

/// Sends to the main window only: the overlay has no use for these.
fn emit(app: &AppHandle, event: MeetingEvent) {
    if let Err(error) = event.emit_to(app, "main") {
        log::warn!("Failed to emit meeting event: {}", error);
    }
}

/// One frame from the microphone, marked with whether the meeting was
/// paused when it was captured (so Pause and Resume act at the moment they
/// were pressed even when the writer is behind).
struct Captured {
    samples: Vec<f32>,
    paused: bool,
}

/// State shared by a meeting's threads.
struct Shared {
    /// None in tests: nothing is sent to a window.
    app: Option<AppHandle>,
    dir: PathBuf,
    manifest: Mutex<MeetingManifest>,
    /// Serializes record saves, so a later save never loses to an earlier
    /// one; held while writing to disk instead of `manifest`.
    save_lock: Mutex<()>,
    /// The writer's queue. The audio callback sends through it; Stop takes
    /// it, which ends the writer even if the microphone will not close.
    frame_tx: Mutex<Option<SyncSender<Captured>>>,
    stop: AtomicBool,
    paused: AtomicBool,
    /// Frames delivered by the microphone (heartbeat).
    frames_seen: AtomicU64,
    /// Samples saved, over all audio files.
    written_samples: AtomicU64,
    /// Samples lost before reaching a file.
    dropped_samples: AtomicU64,
    live_skipped_samples: AtomicU64,
    /// Level as f32 bits.
    rms_db: AtomicU64,
    peak_db: AtomicU64,
    zero_run_samples: AtomicU64,
    /// Samples seen (saved or not), and that count when sound was last heard.
    seen_samples: AtomicU64,
    last_sound_at: AtomicU64,
    mic_lost: AtomicBool,
    /// Why saving is failing, while it is; the writer keeps retrying.
    write_error: Mutex<Option<String>>,
    live: Mutex<LiveStatus>,
    segments: Mutex<Vec<TranscriptSegment>>,
    free_bytes: Mutex<Option<u64>>,
}

impl Shared {
    fn new(app: Option<AppHandle>, dir: PathBuf, manifest: MeetingManifest) -> Self {
        let free_bytes = power::free_space(&dir);
        Self {
            app,
            dir,
            manifest: Mutex::new(manifest),
            save_lock: Mutex::new(()),
            frame_tx: Mutex::new(None),
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            frames_seen: AtomicU64::new(0),
            written_samples: AtomicU64::new(0),
            dropped_samples: AtomicU64::new(0),
            live_skipped_samples: AtomicU64::new(0),
            rms_db: AtomicU64::new(u64::from(level::FLOOR_DB.to_bits())),
            peak_db: AtomicU64::new(u64::from(level::FLOOR_DB.to_bits())),
            zero_run_samples: AtomicU64::new(0),
            seen_samples: AtomicU64::new(0),
            last_sound_at: AtomicU64::new(0),
            mic_lost: AtomicBool::new(false),
            write_error: Mutex::new(None),
            live: Mutex::new(LiveStatus::Off),
            segments: Mutex::new(Vec::new()),
            free_bytes: Mutex::new(free_bytes),
        }
    }

    /// Called by the audio callback for every frame. Never waits.
    fn capture(&self, frame: &[f32]) {
        self.frames_seen.fetch_add(1, Ordering::SeqCst);
        let sent = self.frame_tx.lock().unwrap().as_ref().map(|tx| {
            tx.try_send(Captured {
                samples: frame.to_vec(),
                paused: self.paused.load(Ordering::SeqCst),
            })
        });
        if matches!(sent, Some(Err(_))) {
            self.dropped_samples
                .fetch_add(frame.len() as u64, Ordering::Relaxed);
        }
    }

    /// Closes the writer's queue: the writer saves what is queued and ends.
    fn close_queue(&self) {
        self.frame_tx.lock().unwrap().take();
    }

    fn recorded_ms(&self) -> u64 {
        wav::samples_to_ms(self.written_samples.load(Ordering::SeqCst))
    }

    fn set_level(&self, level: level::Level) {
        self.rms_db
            .store(u64::from(level.rms_db.to_bits()), Ordering::Relaxed);
        self.peak_db
            .store(u64::from(level.peak_db.to_bits()), Ordering::Relaxed);
    }

    fn level(&self) -> (f32, f32) {
        let read = |bits: &AtomicU64| f32::from_bits(bits.load(Ordering::Relaxed) as u32);
        (read(&self.rms_db), read(&self.peak_db))
    }

    fn signal(&self) -> SignalState {
        if self.mic_lost.load(Ordering::SeqCst) {
            SignalState::Lost
        } else if self.zero_run_samples.load(Ordering::Relaxed) >= NO_SIGNAL_SAMPLES {
            SignalState::NoSignal
        } else if self
            .seen_samples
            .load(Ordering::Relaxed)
            .saturating_sub(self.last_sound_at.load(Ordering::Relaxed))
            >= QUIET_SAMPLES
        {
            SignalState::Quiet
        } else {
            SignalState::Ok
        }
    }

    fn phase(&self) -> RecordingPhase {
        if self.stop.load(Ordering::SeqCst) {
            RecordingPhase::Stopping
        } else if self.write_error.lock().unwrap().is_some() {
            RecordingPhase::Failed
        } else if self.paused.load(Ordering::SeqCst) {
            RecordingPhase::Paused
        } else {
            RecordingPhase::Recording
        }
    }

    fn status(&self) -> MeetingStatus {
        let manifest = self.manifest.lock().unwrap().clone();
        let (rms_db, peak_db) = self.level();
        let free_bytes = *self.free_bytes.lock().unwrap();
        MeetingStatus {
            folder: self.dir.to_string_lossy().into_owned(),
            audio_path: manifest
                .audio_path(&self.dir)
                .to_string_lossy()
                .into_owned(),
            id: manifest.id,
            title: manifest.title,
            started_at: manifest.started_at,
            microphone: manifest.microphone,
            phase: self.phase(),
            signal: self.signal(),
            recorded_ms: self.recorded_ms(),
            rms_db,
            peak_db,
            markers: manifest.markers,
            live: self.live.lock().unwrap().clone(),
            live_skipped_ms: wav::samples_to_ms(self.live_skipped_samples.load(Ordering::Relaxed)),
            dropped_ms: wav::samples_to_ms(self.dropped_samples.load(Ordering::Relaxed)),
            free_bytes,
            low_disk: free_bytes.is_some_and(|free| free < LOW_FREE_BYTES),
            write_error: self.write_error.lock().unwrap().clone(),
        }
    }

    fn emit_status(&self) {
        if let Some(app) = &self.app {
            emit(
                app,
                MeetingEvent::Status {
                    status: Some(self.status()),
                },
            );
        }
    }

    /// Changes the record and saves it, without holding the record's lock
    /// while writing to disk.
    fn update_manifest(&self, change: impl FnOnce(&mut MeetingManifest)) {
        let _saving = self.save_lock.lock().unwrap();
        let snapshot = {
            let mut manifest = self.manifest.lock().unwrap();
            change(&mut manifest);
            manifest.clone()
        };
        if let Err(error) = manifest::save(&self.dir, &snapshot) {
            log::warn!("Failed to save the meeting record: {}", error);
        }
    }

    /// Adds a marker at the current end of the saved audio and saves the
    /// record.
    fn add_marker(&self, kind: MarkerKind, label: Option<String>) -> MeetingMarker {
        let marker = MeetingMarker {
            at_ms: self.recorded_ms(),
            wall_ms: now_ms(),
            kind,
            label,
        };
        self.update_manifest(|manifest| {
            manifest.markers.push(marker.clone());
            manifest.duration_ms = marker.at_ms;
        });
        marker
    }

    /// Saving failed: marks it once per failure, until saving works again.
    fn saving_failed(&self, error: &str, disk_full: bool) {
        let first = {
            let mut write_error = self.write_error.lock().unwrap();
            let first = write_error.is_none();
            *write_error = Some(error.to_string());
            first
        };
        if first {
            log::error!("Meeting audio could not be saved: {}", error);
            self.add_marker(
                if disk_full {
                    MarkerKind::DiskFull
                } else {
                    MarkerKind::WriteFailed
                },
                Some(error.to_string()),
            );
            self.emit_status();
        }
    }

    fn saving_resumed(&self, file_name: Option<String>) {
        self.write_error.lock().unwrap().take();
        log::info!("Meeting audio is being saved again");
        if let Some(file_name) = file_name {
            self.update_manifest(|manifest| manifest.extra_audio_files.push(file_name));
        }
        self.add_marker(MarkerKind::WriteResumed, None);
        self.emit_status();
    }
}

/// The writer thread's file: the current audio file, and audio held in
/// memory while saving fails.
struct AudioSink<'a> {
    shared: &'a Shared,
    live: Option<SyncSender<LiveFrame>>,
    current: Option<(wav::DurableWav, PathBuf)>,
    /// Number of the file being written (1 = the first).
    part: u32,
    backlog: std::collections::VecDeque<Vec<f32>>,
    backlog_samples: usize,
    next_retry: Instant,
    last_checkpoint: Instant,
}

impl AudioSink<'_> {
    fn written(&self) -> u64 {
        self.shared.written_samples.load(Ordering::SeqCst)
    }

    /// Writes one frame, or keeps it in memory while saving fails.
    fn push(&mut self, frame: Vec<f32>) {
        if self.current.is_none() || !self.backlog.is_empty() {
            self.hold(frame);
            return;
        }
        self.write(frame);
    }

    fn write(&mut self, frame: Vec<f32>) {
        let at_sample = self.written();
        let Some((audio, _)) = self.current.as_mut() else {
            self.hold(frame);
            return;
        };
        match audio.write(&frame) {
            Ok(()) => {
                self.shared
                    .written_samples
                    .fetch_add(frame.len() as u64, Ordering::SeqCst);
                if let Some(live) = &self.live {
                    let len = frame.len() as u64;
                    if live
                        .try_send(LiveFrame {
                            samples: frame,
                            at_sample,
                        })
                        .is_err()
                    {
                        self.shared
                            .live_skipped_samples
                            .fetch_add(len, Ordering::Relaxed);
                    }
                }
            }
            Err(error) => {
                self.give_up_file(&error);
                self.hold(frame);
            }
        }
    }

    fn hold(&mut self, frame: Vec<f32>) {
        self.backlog_samples += frame.len();
        self.backlog.push_back(frame);
        while self.backlog_samples > MAX_BACKLOG_SAMPLES {
            let Some(oldest) = self.backlog.pop_front() else {
                break;
            };
            self.backlog_samples -= oldest.len();
            self.shared
                .dropped_samples
                .fetch_add(oldest.len() as u64, Ordering::Relaxed);
        }
    }

    /// Stops using the current file after an error; the audio in it stays
    /// readable. A new file is tried after [`WRITE_RETRY`].
    fn give_up_file(&mut self, error: &io::Error) {
        if let Some((audio, path)) = self.current.take() {
            if let Err(repair_error) = audio.abandon(&path) {
                log::warn!("Could not repair {:?}: {}", path, repair_error);
            }
        }
        self.next_retry = Instant::now() + WRITE_RETRY;
        self.shared.saving_failed(
            &error.to_string(),
            error.kind() == io::ErrorKind::StorageFull,
        );
    }

    /// Starts a follow-on file and writes the audio held in memory.
    fn retry(&mut self) {
        if self.current.is_some() || Instant::now() < self.next_retry {
            return;
        }
        let name = self
            .shared
            .manifest
            .lock()
            .unwrap()
            .part_file_name(self.part + 1);
        let path = self.shared.dir.join(&name);
        match wav::DurableWav::create(&path) {
            Ok(audio) => {
                self.part += 1;
                self.current = Some((audio, path));
                self.last_checkpoint = Instant::now();
                self.shared.saving_resumed(Some(name));
                while let Some(frame) = self.backlog.pop_front() {
                    self.backlog_samples -= frame.len();
                    self.write(frame);
                    if self.current.is_none() {
                        break;
                    }
                }
            }
            Err(error) => {
                self.next_retry = Instant::now() + WRITE_RETRY;
                self.shared.saving_failed(
                    &error.to_string(),
                    error.kind() == io::ErrorKind::StorageFull,
                );
            }
        }
    }

    fn checkpoint(&mut self, force: bool) {
        if !force && self.last_checkpoint.elapsed() < CHECKPOINT {
            return;
        }
        self.last_checkpoint = Instant::now();
        let result = match self.current.as_mut() {
            Some((audio, _)) => audio.checkpoint(),
            None => return,
        };
        if let Err(error) = result {
            self.give_up_file(&error);
        }
    }

    fn finish(mut self) {
        if !self.backlog.is_empty() {
            // One last try for the audio held in memory.
            self.next_retry = Instant::now();
            self.retry();
        }
        if let Some((audio, path)) = self.current.take() {
            if let Err(error) = audio.finish() {
                log::warn!("Finishing {:?} failed: {}", path, error);
            }
        }
        if self.backlog_samples > 0 {
            log::error!(
                "{} samples of meeting audio could not be saved",
                self.backlog_samples
            );
            self.shared
                .dropped_samples
                .fetch_add(self.backlog_samples as u64, Ordering::Relaxed);
        }
    }
}

fn run_writer(
    shared: Arc<Shared>,
    audio: wav::DurableWav,
    audio_path: PathBuf,
    frames: Receiver<Captured>,
    live: Option<SyncSender<LiveFrame>>,
) {
    let mut meter = LevelMeter::new(1_600);
    let mut sink = AudioSink {
        shared: &shared,
        live,
        current: Some((audio, audio_path)),
        part: 1,
        backlog: std::collections::VecDeque::new(),
        backlog_samples: 0,
        next_retry: Instant::now(),
        last_checkpoint: Instant::now(),
    };
    loop {
        match frames.recv_timeout(Duration::from_millis(250)) {
            Ok(frame) => {
                let len = frame.samples.len() as u64;
                let seen = shared.seen_samples.fetch_add(len, Ordering::Relaxed) + len;
                if let Some(level) = meter.push(&frame.samples) {
                    shared.set_level(level);
                    if level.peak_db >= SOUND_DB {
                        shared.last_sound_at.store(seen, Ordering::Relaxed);
                    }
                }
                shared
                    .zero_run_samples
                    .store(meter.zero_run_samples(), Ordering::Relaxed);
                if !frame.paused {
                    sink.push(frame.samples);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        sink.retry();
        sink.checkpoint(false);
    }
    sink.finish();
}

/// Waits up to `limit` for a thread; past that it is left running.
fn join_within<T>(handle: JoinHandle<T>, limit: Duration, name: &str) -> bool {
    let deadline = Instant::now() + limit;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            log::warn!("Meeting {} did not finish in time; leaving it", name);
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = handle.join();
    true
}

fn run_supervisor(shared: Arc<Shared>, mut recorder: AudioRecorder, device_name: Option<String>) {
    let _awake = power::KeepAwake::start();
    let mut last_frames = shared.frames_seen.load(Ordering::SeqCst);
    let mut last_frame_at = Instant::now();
    let mut last_wall = SystemTime::now();
    let mut next_retry = Instant::now();
    let mut next_disk_check = Instant::now();
    while !shared.stop.load(Ordering::SeqCst) {
        std::thread::sleep(TICK);

        let wall = SystemTime::now();
        if wall
            .duration_since(last_wall)
            .is_ok_and(|elapsed| elapsed >= SLEEP_GAP)
        {
            log::warn!("Meeting recording: the computer was asleep or stalled");
            shared.add_marker(MarkerKind::Gap, None);
            // Give a stream that survived the sleep time to deliver again.
            last_frame_at = Instant::now();
        }
        last_wall = wall;

        let frames = shared.frames_seen.load(Ordering::SeqCst);
        let lost = shared.mic_lost.load(Ordering::SeqCst);
        if frames != last_frames {
            last_frames = frames;
            last_frame_at = Instant::now();
            if lost {
                log::info!("Meeting microphone is back");
                shared.mic_lost.store(false, Ordering::SeqCst);
                shared.add_marker(MarkerKind::MicBack, None);
            }
        } else if !lost
            && (last_frame_at.elapsed() >= MIC_STALL
                || (recorder.needs_reopen() && last_frame_at.elapsed() >= TICK * 2))
        {
            log::warn!("Meeting microphone stopped delivering audio");
            shared.mic_lost.store(true, Ordering::SeqCst);
            shared.add_marker(MarkerKind::MicLost, None);
            next_retry = Instant::now();
        }
        if shared.mic_lost.load(Ordering::SeqCst) && Instant::now() >= next_retry {
            match open_recorder(&mut recorder, &device_name) {
                Ok(_) => {
                    log::info!("Reopened the meeting microphone");
                    last_frame_at = Instant::now();
                    // Slow devices (Bluetooth) take seconds to send their
                    // first audio: don't reopen again before then.
                    next_retry = Instant::now() + MIC_START_TIMEOUT + MIC_RETRY;
                }
                Err(error) => {
                    log::debug!("Meeting microphone not back yet: {}", error);
                    next_retry = Instant::now() + MIC_RETRY;
                }
            }
        }

        if Instant::now() >= next_disk_check {
            next_disk_check = Instant::now() + Duration::from_secs(10);
            *shared.free_bytes.lock().unwrap() = power::free_space(&shared.dir);
        }
        shared.emit_status();
    }
    // Stop flushes the last captured audio through the callback; Stop then
    // closes the writer's queue whether or not this returns.
    let _ = recorder.stop();
    let _ = recorder.close();
}

/// How long Stop waits for each thread.
struct StopLimits {
    supervisor: Duration,
    writer: Duration,
    live: Duration,
}

const STOP_LIMITS: StopLimits = StopLimits {
    supervisor: Duration::from_secs(6),
    writer: Duration::from_secs(20),
    live: Duration::from_secs(5),
};

/// At exit the system may allow only a few seconds.
const EXIT_LIMITS: StopLimits = StopLimits {
    supervisor: Duration::from_millis(2_500),
    writer: Duration::from_secs(2),
    live: Duration::ZERO,
};

/// The running meeting's threads.
struct Session {
    shared: Arc<Shared>,
    supervisor: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    live: Option<(JoinHandle<()>, WorkToken)>,
}

pub struct MeetingManager {
    app: AppHandle,
    session: Mutex<Option<Session>>,
    /// A start is opening the microphone (the session lock is not held).
    starting: AtomicBool,
    /// Held for the whole of a Stop.
    stop_lock: Mutex<()>,
    /// Set while a microphone check runs; cleared to stop it early.
    check_running: AtomicBool,
    check_cancel: AtomicBool,
    /// Meetings closed by recovery at this start, not yet shown.
    recovered: Mutex<Vec<String>>,
}

/// Finds the input device named `name`, or the system default for None.
/// Never falls back from a named device to another one.
fn resolve_device(name: &Option<String>) -> Result<cpal::Device, String> {
    let devices = list_input_devices().map_err(|e| format!("Failed to list microphones: {}", e))?;
    let found = match name {
        Some(name) => devices.into_iter().find(|d| &d.name == name),
        None => devices.into_iter().find(|d| d.is_default),
    };
    match (found, name) {
        (Some(device), _) => Ok(device.device),
        (None, Some(name)) => Err(format!(
            "The microphone \"{}\" is not connected. Connect it, or choose another microphone in Settings.",
            name
        )),
        (None, None) => Err("No microphone was found.".to_string()),
    }
}

fn open_recorder(
    recorder: &mut AudioRecorder,
    device_name: &Option<String>,
) -> Result<Receiver<()>, String> {
    let device = resolve_device(device_name)?;
    let _ = recorder.close();
    recorder
        .open(Some(device))
        .map_err(|e| format!("Failed to open the microphone: {}", e))?;
    recorder
        .start(VadPolicy::Disabled)
        .map_err(|e| format!("Failed to start the microphone: {}", e))
}

impl MeetingManager {
    pub fn new(app: &AppHandle) -> Self {
        Self {
            app: app.clone(),
            session: Mutex::new(None),
            starting: AtomicBool::new(false),
            stop_lock: Mutex::new(()),
            check_running: AtomicBool::new(false),
            check_cancel: AtomicBool::new(false),
            recovered: Mutex::new(Vec::new()),
        }
    }

    /// Closes meetings left recording by a crash, power loss or forced quit.
    /// Call once at startup, before any meeting can start.
    pub fn recover(&self) {
        let Ok(root) = meetings_dir(&self.app) else {
            return;
        };
        for (dir, mut manifest) in manifest::list(&root) {
            if manifest.state != MeetingFileState::Recording {
                continue;
            }
            match manifest::recover(&dir, &mut manifest) {
                Ok(true) => {
                    log::warn!(
                        "Recovered meeting {} interrupted after {} ms of audio",
                        manifest.id,
                        manifest.duration_ms
                    );
                    self.recovered.lock().unwrap().push(manifest.id);
                }
                Ok(false) => {}
                Err(error) => log::error!("Failed to recover meeting {:?}: {}", dir, error),
            }
        }
    }

    /// Meetings recovered at this start that the window has not shown yet.
    pub fn take_recovered(&self) -> Vec<String> {
        std::mem::take(&mut *self.recovered.lock().unwrap())
    }

    pub fn has_recovered(&self) -> bool {
        !self.recovered.lock().unwrap().is_empty()
    }

    fn shared(&self) -> Option<Arc<Shared>> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|session| Arc::clone(&session.shared))
    }

    pub fn status(&self) -> Option<MeetingStatus> {
        self.shared().map(|shared| shared.status())
    }

    pub fn live_segments(&self) -> Vec<TranscriptSegment> {
        self.shared()
            .map(|shared| shared.segments.lock().unwrap().clone())
            .unwrap_or_default()
    }

    /// Records 10 seconds from the meeting microphone, reporting levels as it
    /// goes, then judges the sound and transcribes it. Blocking.
    pub fn check_microphone(&self) -> Result<MicCheckResult, String> {
        if is_active() || self.starting.load(Ordering::SeqCst) {
            return Err("A meeting is being recorded.".to_string());
        }
        if self.check_running.swap(true, Ordering::SeqCst) {
            return Err("A microphone check is already running.".to_string());
        }
        struct Running<'a>(&'a AtomicBool);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _running = Running(&self.check_running);
        self.check_cancel.store(false, Ordering::SeqCst);

        let settings = get_settings(&self.app);
        let microphone = settings.selected_microphone.clone();
        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(WRITER_QUEUE_FRAMES);
        let mut recorder = AudioRecorder::new()
            .map_err(|e| e.to_string())?
            .with_selected_channel(settings.selected_channel)
            .without_retained_samples()
            .with_audio_callback(move |frame| {
                let _ = tx.try_send(frame.to_vec());
            });
        let ready = open_recorder(&mut recorder, &microphone)?;
        if ready.recv_timeout(MIC_START_TIMEOUT).is_err() {
            let _ = recorder.close();
            return Err("The microphone did not send any audio.".to_string());
        }

        let mut samples: Vec<f32> = Vec::with_capacity(16 * MIC_CHECK_MS as usize);
        let mut meter = LevelMeter::new(4_000);
        let started = Instant::now();
        while samples.len() < 16 * MIC_CHECK_MS as usize
            && !self.check_cancel.load(Ordering::SeqCst)
        {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(frame) => {
                    if let Some(level) = meter.push(&frame) {
                        emit(
                            &self.app,
                            MeetingEvent::CheckLevel {
                                rms_db: level.rms_db,
                                peak_db: level.peak_db,
                                elapsed_ms: samples.len() as u64 / 16,
                            },
                        );
                    }
                    samples.extend_from_slice(&frame);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if started.elapsed() > Duration::from_secs(15) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = recorder.stop();
        let _ = recorder.close();
        if self.check_cancel.load(Ordering::SeqCst) {
            return Err("The microphone check was stopped.".to_string());
        }

        let stats = level::judge_mic_check(&samples);
        let heard = if stats.verdict == level::MicCheckVerdict::NoSignal {
            Some(String::new())
        } else {
            let tm = self.app.state::<Arc<TranscriptionManager>>();
            tm.initiate_model_load();
            let _hold = tm.hold_for_dictation();
            match tm.transcribe(samples) {
                Ok(text) => Some(text.trim().to_string()),
                Err(error) => {
                    log::warn!("Microphone check transcription failed: {}", error);
                    None
                }
            }
        };
        Ok(MicCheckResult {
            stats,
            heard,
            microphone,
        })
    }

    pub fn cancel_check(&self) {
        self.check_cancel.store(true, Ordering::SeqCst);
    }

    /// Starts recording a meeting. Blocking (opens the microphone), without
    /// holding the session lock, so the window stays responsive.
    pub fn start(&self, options: StartMeetingOptions) -> Result<MeetingStatus, String> {
        if !options.consent_confirmed {
            return Err("Confirm that everyone agreed to be recorded.".to_string());
        }
        {
            let session = self.session.lock().unwrap();
            // ACTIVE also covers a meeting that is still saving after Stop.
            if session.is_some() || is_active() || self.starting.swap(true, Ordering::SeqCst) {
                return Err("A meeting is already being recorded.".to_string());
            }
        }
        let result = self.start_recording(options);
        self.starting.store(false, Ordering::SeqCst);
        result
    }

    fn start_recording(&self, options: StartMeetingOptions) -> Result<MeetingStatus, String> {
        if self.check_running.load(Ordering::SeqCst) {
            return Err("Wait for the microphone check to finish.".to_string());
        }
        if self
            .app
            .try_state::<Arc<AudioRecordingManager>>()
            .is_some_and(|audio| audio.is_recording())
        {
            return Err("Finish the dictation first.".to_string());
        }

        let settings = get_settings(&self.app);
        let microphone = settings.selected_microphone.clone();
        let device = resolve_device(&microphone)?;

        let root = meetings_dir(&self.app)?;
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("Failed to create the meetings folder: {}", e))?;
        if let Some(free) = power::free_space(&root) {
            if free < MIN_FREE_BYTES_TO_START {
                return Err(format!(
                    "Not enough disk space: {} MB free, at least {} MB needed.",
                    free / (1024 * 1024),
                    MIN_FREE_BYTES_TO_START / (1024 * 1024)
                ));
            }
        }

        let now = chrono::Local::now();
        let base_id = now.format("%Y-%m-%d_%H-%M-%S").to_string();
        let (id, dir) = (1..100)
            .map(|n| {
                if n == 1 {
                    base_id.clone()
                } else {
                    format!("{base_id}_{n}")
                }
            })
            .find_map(|id| {
                let dir = root.join(&id);
                std::fs::create_dir(&dir).ok().map(|_| (id, dir))
            })
            .ok_or_else(|| "Failed to create the meeting folder.".to_string())?;
        let stamp = now.format("%Y-%m-%d %H-%M").to_string();
        let started_at = now.timestamp_millis();
        // Consent was confirmed by the Start click that led here.
        let manifest = MeetingManifest::new(
            id.clone(),
            options.title.trim().to_string(),
            manifest::audio_file_name(&stamp, &options.title),
            started_at,
            microphone.clone(),
            started_at,
            options.label_speakers,
        );

        let cleanup = |dir: &Path| {
            let _ = std::fs::remove_dir_all(dir);
        };
        if let Err(error) = manifest::save(&dir, &manifest) {
            cleanup(&dir);
            return Err(format!("Failed to save the meeting record: {}", error));
        }
        let audio_path = manifest.audio_path(&dir);
        let audio = match wav::DurableWav::create(&audio_path) {
            Ok(audio) => audio,
            Err(error) => {
                cleanup(&dir);
                return Err(format!("Failed to create the audio file: {}", error));
            }
        };

        let (frame_tx, frame_rx) = mpsc::sync_channel::<Captured>(WRITER_QUEUE_FRAMES);
        let shared = Arc::new(Shared::new(
            Some(self.app.clone()),
            dir.clone(),
            manifest.clone(),
        ));
        *shared.frame_tx.lock().unwrap() = Some(frame_tx);

        let callback_shared = Arc::clone(&shared);
        let mut recorder = match AudioRecorder::new() {
            Ok(recorder) => recorder,
            Err(error) => {
                cleanup(&dir);
                return Err(error.to_string());
            }
        }
        .with_selected_channel(settings.selected_channel)
        .without_retained_samples()
        .with_audio_callback(move |frame| callback_shared.capture(frame));

        // The live transcript, then the writer, so the queues drain from the
        // first frame.
        let (live_handle, live_tx) = if options.live_transcript {
            match self.spawn_live(&shared) {
                Some((handle, token, tx)) => (Some((handle, token)), Some(tx)),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let stop_live = |live_handle: Option<(JoinHandle<()>, WorkToken)>| {
            if let Some((handle, token)) = live_handle {
                token.cancel();
                // A model load can't be cancelled; don't wait for it.
                drop(handle);
            }
        };

        let writer_shared = Arc::clone(&shared);
        let writer = std::thread::Builder::new()
            .name("meeting-writer".into())
            .spawn(move || {
                let shared = Arc::clone(&writer_shared);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_writer(writer_shared, audio, audio_path, frame_rx, live_tx)
                }));
                if result.is_err() {
                    shared.saving_failed("the audio writer stopped unexpectedly", false);
                }
            });
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                stop_live(live_handle);
                cleanup(&dir);
                return Err(error.to_string());
            }
        };
        let abort = |shared: &Shared,
                     writer: JoinHandle<()>,
                     live_handle: Option<(JoinHandle<()>, WorkToken)>| {
            shared.close_queue();
            join_within(writer, STOP_LIMITS.writer, "writer");
            stop_live(live_handle);
            cleanup(&dir);
        };

        let ready = recorder
            .open(Some(device))
            .map_err(|e| format!("Failed to open the microphone: {}", e))
            .and_then(|_| {
                recorder
                    .start(VadPolicy::Disabled)
                    .map_err(|e| format!("Failed to start the microphone: {}", e))
            })
            .and_then(|ready| {
                ready
                    .recv_timeout(MIC_START_TIMEOUT)
                    .map_err(|_| "The microphone did not send any audio.".to_string())
            });
        if let Err(error) = ready {
            let _ = recorder.close();
            abort(&shared, writer, live_handle);
            return Err(error);
        }

        let supervisor_shared = Arc::clone(&shared);
        let device_name = microphone.clone();
        let supervisor = match std::thread::Builder::new()
            .name("meeting-supervisor".into())
            .spawn(move || run_supervisor(supervisor_shared, recorder, device_name))
        {
            Ok(supervisor) => supervisor,
            Err(error) => {
                abort(&shared, writer, live_handle);
                return Err(error.to_string());
            }
        };

        ACTIVE.store(true, Ordering::SeqCst);
        QUIT_CONFIRMED.store(false, Ordering::SeqCst);
        log::info!("Meeting {} started in {:?}", id, dir);
        *self.session.lock().unwrap() = Some(Session {
            shared: Arc::clone(&shared),
            supervisor: Some(supervisor),
            writer: Some(writer),
            live: live_handle,
        });
        crate::tray::set_meeting_active(&self.app, true);
        let status = shared.status();
        emit(
            &self.app,
            MeetingEvent::Status {
                status: Some(status.clone()),
            },
        );
        Ok(status)
    }

    fn spawn_live(
        &self,
        shared: &Arc<Shared>,
    ) -> Option<(JoinHandle<()>, WorkToken, SyncSender<LiveFrame>)> {
        let (live_tx, live_rx) = mpsc::sync_channel::<LiveFrame>(LIVE_QUEUE_FRAMES);
        let token = WorkToken::new();
        let live_shared = Arc::clone(shared);
        let live_token = token.clone();
        let app = self.app.clone();
        let handle = std::thread::Builder::new()
            .name("meeting-live".into())
            .spawn(move || {
                let meeting_id = live_shared.manifest.lock().unwrap().id.clone();
                let transcript_path = live_shared.dir.join("live-transcript.txt");
                live::run(app, live_rx, live_token, |event| match event {
                    LiveEvent::Status(status) => {
                        *live_shared.live.lock().unwrap() = status;
                        live_shared.emit_status();
                    }
                    LiveEvent::Segments(segments) => {
                        append_live_transcript(&transcript_path, &segments);
                        live_shared
                            .segments
                            .lock()
                            .unwrap()
                            .extend(segments.iter().cloned());
                        if let Some(app) = &live_shared.app {
                            emit(
                                app,
                                MeetingEvent::Segments {
                                    meeting_id: meeting_id.clone(),
                                    segments,
                                },
                            );
                        }
                    }
                });
            });
        match handle {
            Ok(handle) => Some((handle, token, live_tx)),
            Err(error) => {
                log::warn!("Could not start the live transcript: {}", error);
                None
            }
        }
    }

    pub fn set_paused(&self, paused: bool) -> Result<MeetingStatus, String> {
        let shared = self
            .shared()
            .ok_or_else(|| "No meeting is being recorded.".to_string())?;
        if shared.paused.swap(paused, Ordering::SeqCst) != paused {
            shared.add_marker(
                if paused {
                    MarkerKind::Paused
                } else {
                    MarkerKind::Resumed
                },
                None,
            );
        }
        shared.emit_status();
        Ok(shared.status())
    }

    pub fn add_note(&self, label: Option<String>) -> Result<MeetingMarker, String> {
        let shared = self
            .shared()
            .ok_or_else(|| "No meeting is being recorded.".to_string())?;
        let label = label
            .map(|label| label.trim().chars().take(200).collect::<String>())
            .filter(|label| !label.is_empty());
        let marker = shared.add_marker(MarkerKind::Note, label);
        shared.emit_status();
        Ok(marker)
    }

    /// Stops the meeting, saves the audio and, unless `transcribe` is false,
    /// sends it to the transcription queue. Blocking (a few seconds).
    pub fn stop(&self, transcribe: bool) -> Result<MeetingSummary, String> {
        self.stop_within(transcribe, &STOP_LIMITS)
    }

    fn stop_within(&self, transcribe: bool, limits: &StopLimits) -> Result<MeetingSummary, String> {
        // A second Stop (or the exit handler) waits for the first.
        let _stopping = self.stop_lock.lock().unwrap();
        let Some(mut session) = self.session.lock().unwrap().take() else {
            return Err("No meeting is being recorded.".to_string());
        };
        let shared = Arc::clone(&session.shared);
        shared.stop.store(true, Ordering::SeqCst);
        shared.emit_status();

        // 1. The supervisor stops the microphone; its last audio still
        //    reaches the writer. A driver that hangs is left behind.
        if let Some(supervisor) = session.supervisor.take() {
            join_within(supervisor, limits.supervisor, "microphone");
        }
        // 2. The writer saves what is queued and finishes its file.
        shared.close_queue();
        let saved = session
            .writer
            .take()
            .is_none_or(|writer| join_within(writer, limits.writer, "writer"));

        // 3. The record, before anything slower.
        let mut manifest = shared.manifest.lock().unwrap().clone();
        let mut samples = 0;
        for path in manifest.audio_paths(&shared.dir) {
            // Makes each header match its file, even after a failed write.
            match wav::repair(&path) {
                Ok(part) => samples += part,
                Err(error) => log::warn!("Could not check {:?}: {}", path, error),
            }
        }
        manifest.duration_ms = wav::samples_to_ms(samples);
        manifest.ended_at = Some(now_ms());
        // A writer still running is repaired again at the next start.
        manifest.state = if saved {
            MeetingFileState::Stopped
        } else {
            MeetingFileState::Recording
        };
        {
            let _saving = shared.save_lock.lock().unwrap();
            if let Err(error) = manifest::save(&shared.dir, &manifest) {
                log::error!("Failed to save the meeting record: {}", error);
            }
        }
        ACTIVE.store(false, Ordering::SeqCst);
        log::info!(
            "Meeting {} stopped with {} ms of audio",
            manifest.id,
            manifest.duration_ms
        );

        // 4. The live transcript last: a model load can't be interrupted.
        if let Some((handle, token)) = session.live.take() {
            token.cancel();
            if let Some(tm) = self.app.try_state::<Arc<TranscriptionManager>>() {
                tm.cancel_file_work(&token);
            }
            if !limits.live.is_zero() {
                join_within(handle, limits.live, "live transcript");
            }
        }

        crate::tray::set_meeting_active(&self.app, false);
        emit(&self.app, MeetingEvent::Status { status: None });

        if transcribe && saved && samples > 0 {
            if let Err(error) = self.enqueue(&shared.dir, &mut manifest) {
                log::error!("Failed to queue the meeting for transcription: {}", error);
            }
        }
        let summary = self.summary(&shared.dir, manifest);
        emit(
            &self.app,
            MeetingEvent::Stopped {
                meeting: summary.clone(),
            },
        );
        Ok(summary)
    }

    /// Saves the running meeting without transcribing it, for app exit.
    /// Waits briefly for a Stop already in progress.
    pub fn shutdown(&self) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while self.stop_lock.try_lock().is_err() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if is_active() {
            let _ = self.stop_within(false, &EXIT_LIMITS);
        }
    }

    fn enqueue(&self, dir: &Path, manifest: &mut MeetingManifest) -> Result<(), String> {
        let queue = self
            .app
            .try_state::<Arc<FileTranscriptionManager>>()
            .ok_or_else(|| "The transcription queue is not running.".to_string())?;
        let paths: Vec<String> = manifest
            .audio_paths(dir)
            .into_iter()
            .filter(|path| path.is_file())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        if paths.is_empty() {
            return Err("The meeting has no audio file.".to_string());
        }
        let jobs = queue.enqueue(
            paths,
            FileTranscriptionOptions {
                save_to_history: true,
                label_speakers: manifest.label_speakers,
            },
        );
        if let Some(error) = jobs.first().and_then(|job| job.error.as_ref()) {
            return Err(error.message.clone());
        }
        manifest.state = MeetingFileState::Queued;
        manifest::save(dir, manifest).map_err(|e| e.to_string())
    }

    fn summary(&self, dir: &Path, manifest: MeetingManifest) -> MeetingSummary {
        let audio_path = manifest.audio_path(dir);
        let audio = audio_path.to_string_lossy().into_owned();
        let history_id = self
            .app
            .try_state::<Arc<HistoryManager>>()
            .and_then(|history| history.file_entry_for_source(&audio).ok().flatten());
        MeetingSummary {
            folder: dir.to_string_lossy().into_owned(),
            audio_exists: audio_path.is_file(),
            audio_path: audio,
            history_id,
            manifest,
        }
    }

    /// Past meetings, newest first (the running one excluded).
    pub fn list(&self) -> Result<Vec<MeetingSummary>, String> {
        let root = meetings_dir(&self.app)?;
        let running = self
            .shared()
            .map(|shared| shared.manifest.lock().unwrap().id.clone());
        Ok(manifest::list(&root)
            .into_iter()
            .filter(|(_, manifest)| Some(&manifest.id) != running.as_ref())
            .map(|(dir, manifest)| self.summary(&dir, manifest))
            .collect())
    }

    /// Sends a past meeting's audio to the transcription queue.
    pub fn transcribe(&self, id: &str) -> Result<MeetingSummary, String> {
        let root = meetings_dir(&self.app)?;
        let (dir, mut manifest) = manifest::list(&root)
            .into_iter()
            .find(|(_, manifest)| manifest.id == id)
            .ok_or_else(|| "The meeting was not found.".to_string())?;
        if manifest.state == MeetingFileState::Recording {
            return Err("The meeting is still being recorded.".to_string());
        }
        self.enqueue(&dir, &mut manifest)?;
        Ok(self.summary(&dir, manifest))
    }
}

fn append_live_transcript(path: &Path, segments: &[TranscriptSegment]) {
    use std::io::Write;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            for segment in segments {
                let seconds = segment.start_ms / 1000;
                writeln!(
                    file,
                    "[{:02}:{:02}:{:02}] {}",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60,
                    segment.text
                )?;
            }
            Ok(())
        });
    if let Err(error) = result {
        log::debug!("Could not append to the live transcript file: {}", error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: f32) -> Vec<f32> {
        vec![value; 480]
    }

    fn captured(value: f32, paused: bool) -> Captured {
        Captured {
            samples: frame(value),
            paused,
        }
    }

    fn sample_manifest() -> MeetingManifest {
        MeetingManifest::new(
            "m".into(),
            String::new(),
            "Meeting.wav".into(),
            0,
            None,
            0,
            false,
        )
    }

    struct Writer {
        shared: Arc<Shared>,
        frames: SyncSender<Captured>,
        handle: JoinHandle<()>,
        audio: PathBuf,
    }

    fn start_writer(live: Option<SyncSender<LiveFrame>>) -> (tempfile::TempDir, Writer) {
        let dir = tempfile::tempdir().unwrap();
        let manifest = sample_manifest();
        let audio = manifest.audio_path(dir.path());
        let wav = wav::DurableWav::create(&audio).unwrap();
        let shared = Arc::new(Shared::new(None, dir.path().to_path_buf(), manifest));
        let (frames, rx) = mpsc::sync_channel(WRITER_QUEUE_FRAMES);
        let writer_shared = Arc::clone(&shared);
        let path = audio.clone();
        let handle = std::thread::spawn(move || run_writer(writer_shared, wav, path, rx, live));
        (
            dir,
            Writer {
                shared,
                frames,
                handle,
                audio,
            },
        )
    }

    fn wait_until(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_written(shared: &Shared, samples: u64) {
        wait_until("the writer", || {
            shared.written_samples.load(Ordering::SeqCst) >= samples
        });
    }

    fn read_samples(path: &Path) -> Vec<i16> {
        hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn writer_saves_audio_skips_paused_frames_and_places_markers() {
        let (_dir, writer) = start_writer(None);
        for _ in 0..100 {
            writer.frames.send(captured(0.25, false)).unwrap();
        }
        wait_for_written(&writer.shared, 48_000);
        writer.shared.add_marker(MarkerKind::Paused, None);
        // Captured while paused: moves the meter, not saved.
        for _ in 0..50 {
            writer.frames.send(captured(0.5, true)).unwrap();
        }
        wait_until("paused frames", || {
            writer.shared.seen_samples.load(Ordering::Relaxed) >= 72_000
        });
        writer.shared.add_marker(MarkerKind::Resumed, None);
        for _ in 0..20 {
            writer.frames.send(captured(-0.25, false)).unwrap();
        }
        wait_for_written(&writer.shared, 57_600);
        let note = writer
            .shared
            .add_marker(MarkerKind::Note, Some("Offer".into()));

        drop(writer.frames);
        writer.handle.join().unwrap();

        let samples = read_samples(&writer.audio);
        assert_eq!(samples.len(), 57_600);
        assert!(samples[..48_000].iter().all(|s| *s > 0));
        assert!(samples[48_000..].iter().all(|s| *s < 0));

        let manifest = manifest::load(&writer.shared.dir).unwrap();
        let positions: Vec<(MarkerKind, u64)> = manifest
            .markers
            .iter()
            .map(|marker| (marker.kind, marker.at_ms))
            .collect();
        assert_eq!(
            positions,
            [
                (MarkerKind::Paused, 3_000),
                (MarkerKind::Resumed, 3_000),
                (MarkerKind::Note, 3_600)
            ]
        );
        assert_eq!(note.label.as_deref(), Some("Offer"));
        assert_eq!(writer.shared.recorded_ms(), 3_600);
    }

    #[test]
    fn capture_marks_frames_paused_when_they_were_recorded() {
        let shared = Shared::new(None, PathBuf::from("."), sample_manifest());
        let (tx, rx) = mpsc::sync_channel(4);
        *shared.frame_tx.lock().unwrap() = Some(tx);
        shared.capture(&frame(0.1));
        shared.paused.store(true, Ordering::SeqCst);
        shared.capture(&frame(0.1));
        let paused: Vec<bool> = rx.try_iter().map(|frame| frame.paused).collect();
        assert_eq!(paused, [false, true]);

        // A closed queue loses nothing silently: frames are counted.
        shared.close_queue();
        shared.capture(&frame(0.1));
        assert_eq!(shared.frames_seen.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn closing_the_queue_ends_the_writer_without_the_microphone() {
        let (_dir, writer) = start_writer(None);
        *writer.shared.frame_tx.lock().unwrap() = Some(writer.frames.clone());
        for _ in 0..10 {
            writer.shared.capture(&frame(0.3));
        }
        // The test's own sender stands in for nothing else holding the queue.
        drop(writer.frames);
        writer.shared.close_queue();
        assert!(join_within(writer.handle, Duration::from_secs(5), "writer"));
        assert_eq!(read_samples(&writer.audio).len(), 4_800);
    }

    #[test]
    fn audio_is_readable_before_the_meeting_ends() {
        let (_dir, writer) = start_writer(None);
        for _ in 0..100 {
            writer.frames.send(captured(0.1, false)).unwrap();
        }
        wait_for_written(&writer.shared, 48_000);
        // The next checkpoint makes it durable and readable as is.
        std::thread::sleep(CHECKPOINT + Duration::from_millis(500));
        assert_eq!(hound::WavReader::open(&writer.audio).unwrap().len(), 48_000);

        drop(writer.frames);
        writer.handle.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_file_is_continued_in_a_follow_on_file() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Shared::new(None, dir.path().to_path_buf(), sample_manifest());
        // /dev/full fails every write with "no space left on device".
        let full = wav::DurableWav::open_existing(Path::new("/dev/full")).unwrap();
        let mut sink = AudioSink {
            shared: &shared,
            live: None,
            current: Some((full, PathBuf::from("/dev/full"))),
            part: 1,
            backlog: std::collections::VecDeque::new(),
            backlog_samples: 0,
            next_retry: Instant::now(),
            last_checkpoint: Instant::now(),
        };
        for _ in 0..40 {
            sink.push(frame(0.2));
        }
        sink.checkpoint(true);
        assert!(sink.current.is_none());
        assert!(shared.write_error.lock().unwrap().is_some());
        assert_eq!(shared.phase(), RecordingPhase::Failed);

        // Audio keeps arriving and is held until the retry.
        for _ in 0..10 {
            sink.push(frame(0.2));
        }
        sink.retry();
        assert!(sink.current.is_none(), "retried before WRITE_RETRY");
        sink.next_retry = Instant::now();
        sink.retry();
        assert!(sink.current.is_some());
        assert!(shared.write_error.lock().unwrap().is_none());
        sink.push(frame(0.2));
        sink.finish();

        let manifest = shared.manifest.lock().unwrap().clone();
        assert_eq!(manifest.extra_audio_files, ["Meeting (part 2).wav"]);
        let kinds: Vec<MarkerKind> = manifest.markers.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, [MarkerKind::DiskFull, MarkerKind::WriteResumed]);
        // Frames that never reached /dev/full's disk plus the held ones
        // went to part 2; whatever the buffered writer had taken first is
        // the only loss.
        let part2 = read_samples(&dir.path().join("Meeting (part 2).wav"));
        assert!(part2.len() >= 11 * 480, "{}", part2.len());
    }

    #[test]
    fn backlog_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Shared::new(None, dir.path().to_path_buf(), sample_manifest());
        let mut sink = AudioSink {
            shared: &shared,
            live: None,
            current: None,
            part: 1,
            backlog: std::collections::VecDeque::new(),
            backlog_samples: 0,
            next_retry: Instant::now() + Duration::from_secs(60),
            last_checkpoint: Instant::now(),
        };
        let frames = MAX_BACKLOG_SAMPLES / 480 + 10;
        for _ in 0..frames {
            sink.push(frame(0.2));
        }
        assert!(sink.backlog_samples <= MAX_BACKLOG_SAMPLES);
        assert_eq!(shared.dropped_samples.load(Ordering::Relaxed), 10 * 480);
    }

    #[test]
    fn live_queue_never_holds_back_the_recording() {
        // A live transcript that never reads: its queue fills, and the
        // recording carries on with every frame.
        let (live_tx, live_rx) = mpsc::sync_channel::<LiveFrame>(10);
        let (_dir, writer) = start_writer(Some(live_tx));
        for _ in 0..200 {
            writer.frames.send(captured(0.2, false)).unwrap();
        }
        wait_for_written(&writer.shared, 96_000);
        drop(writer.frames);
        writer.handle.join().unwrap();

        assert_eq!(
            writer.shared.live_skipped_samples.load(Ordering::Relaxed),
            190 * 480
        );
        let queued: Vec<LiveFrame> = live_rx.try_iter().collect();
        assert_eq!(queued.len(), 10);
        // Positions are where each frame sits in the saved audio.
        assert!(queued
            .iter()
            .enumerate()
            .all(|(i, frame)| frame.at_sample == i as u64 * 480));
    }

    #[test]
    fn signal_state_reports_silence_and_quiet() {
        let (_dir, writer) = start_writer(None);
        // Three seconds of exact zeros: muted or blocked.
        for _ in 0..100 {
            writer.frames.send(captured(0.0, false)).unwrap();
        }
        wait_for_written(&writer.shared, 48_000);
        assert_eq!(writer.shared.signal(), SignalState::NoSignal);

        // Faint but not zero: a working microphone that hears nothing yet.
        for _ in 0..10 {
            writer.frames.send(captured(0.0001, false)).unwrap();
        }
        wait_for_written(&writer.shared, 52_800);
        assert_eq!(writer.shared.signal(), SignalState::Ok);
        writer
            .shared
            .seen_samples
            .fetch_add(QUIET_SAMPLES, Ordering::Relaxed);
        assert_eq!(writer.shared.signal(), SignalState::Quiet);

        writer.shared.mic_lost.store(true, Ordering::SeqCst);
        assert_eq!(writer.shared.signal(), SignalState::Lost);

        drop(writer.frames);
        writer.handle.join().unwrap();
    }

    #[test]
    fn join_within_gives_up_on_a_stuck_thread() {
        let slow = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(500)));
        assert!(!join_within(slow, Duration::from_millis(50), "test"));
        let quick = std::thread::spawn(|| {});
        assert!(join_within(quick, Duration::from_secs(5), "test"));
    }

    #[test]
    fn second_quit_soon_after_goes_through() {
        ACTIVE.store(true, Ordering::SeqCst);
        QUIT_HELD_AT.store(0, Ordering::SeqCst);
        assert!(hold_quit());
        assert!(!hold_quit());
        ACTIVE.store(false, Ordering::SeqCst);
        assert!(!hold_quit());
    }
}
