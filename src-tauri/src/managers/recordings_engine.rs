//! The recordings engine: a second transcribe.cpp supervisor for file jobs
//! whose model for recordings (Transcribe page) differs from the dictation
//! model.
//!
//! It runs its workers at low priority and is loaded only while files are
//! transcribed, so the dictation model stays loaded beside it. Its chunks
//! never take the run gate: they never start while a dictation is active,
//! and a chunk still on the GPU when a dictation's key is released is
//! interrupted and redone afterwards (see
//! `TranscriptionManager::hold_for_dictation`). Only transcribe-cpp models
//! run here: an in-process ONNX engine can't be killed, isolated from
//! crashes or deprioritized.
//!
//! The pure decisions live here so they can be tested without an app; the
//! [`TranscriptionManager`](super::transcription::TranscriptionManager) does
//! the loading and the work.

use crate::engine_supervisor::{EngineSupervisor, WorkerOptions};
use crate::settings::ModelUnloadTimeout;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

/// Which engine a Handy file job transcribes on, decided once when it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileModelSlot {
    /// Dictation's slot: the loaded dictation model, the run gate, and the
    /// job stops when the dictation model is switched (today's path).
    Dictation,
    /// The dedicated recordings engine.
    Own,
}

/// The slot for a job given the model for recordings setting and the
/// dictation model: "" ("Same as dictation") or the dictation model itself
/// share dictation's slot; any other model gets the recordings engine.
pub fn choose_file_model_slot(recordings_setting: &str, selected_model: &str) -> FileModelSlot {
    match recordings_setting.trim() {
        "" => FileModelSlot::Dictation,
        chosen if chosen == selected_model => FileModelSlot::Dictation,
        _ => FileModelSlot::Own,
    }
}

/// Longest an idle recordings model stays loaded.
pub const RECORDINGS_MAX_LINGER: Duration = Duration::from_secs(120);

/// How long an idle recordings model stays loaded: the unload timeout, at
/// most [`RECORDINGS_MAX_LINGER`] ("Never" counts as that too). Recordings
/// come in batches, and holding a second model for longer only costs memory.
pub fn recordings_linger(timeout: ModelUnloadTimeout) -> Duration {
    let max = RECORDINGS_MAX_LINGER.as_secs();
    Duration::from_secs(timeout.to_seconds().map_or(max, |secs| secs.min(max)))
}

/// Whether the recordings model should be unloaded now: it is loaded, no
/// job uses it, and it has been idle for `linger`.
pub fn should_unload_recordings(
    loaded: bool,
    own_jobs: usize,
    idle: Duration,
    linger: Duration,
) -> bool {
    loaded && own_jobs == 0 && idle >= linger
}

/// Headroom the GPU needs beside a model's weights (compute buffers, KV
/// cache, the driver's own use).
const VRAM_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;

/// Free GPU memory a recordings model of `recordings_bytes` needs, when the
/// dictation model (`reserve_bytes`, 0 when it is already on the GPU or
/// won't go there) must still fit too: 15 % over both files plus a fixed
/// headroom. Saturates instead of overflowing.
pub fn vram_need(recordings_bytes: u64, reserve_bytes: u64) -> u64 {
    let weights = u128::from(recordings_bytes) + u128::from(reserve_bytes);
    let need = weights * 115 / 100 + u128::from(VRAM_HEADROOM_BYTES);
    u64::try_from(need).unwrap_or(u64::MAX)
}

/// The recordings engine's state, for the Transcribe page. Emitted when it
/// changes; the dictation model's `model-state-changed` events never carry
/// it, so the footer keeps showing the dictation model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct RecordingsEngineEvent {
    /// The loaded model for recordings, if any.
    pub model_id: Option<String>,
    /// Whether it runs on a GPU.
    pub on_gpu: bool,
    /// It was meant for the GPU, but the GPU had too little free memory for
    /// both models, so it runs on the processor.
    pub gpu_memory_fallback: bool,
}

/// The recordings engine and its bookkeeping.
pub(crate) struct RecordingsEngine {
    /// Low-priority workers; no worker exists until a job loads a model.
    pub(crate) engine: EngineSupervisor,
    /// The model the engine holds (`None` while loading or unloaded).
    pub(crate) model_id: Mutex<Option<String>>,
    /// Held across a recordings model load, so only one runs at a time (and
    /// a deletion can keep a load from reopening the file it deletes).
    pub(crate) load_lock: Mutex<()>,
    /// An accelerator change: the next job start loads the model afresh.
    pub(crate) reload_on_next_use: AtomicBool,
    /// Unix ms of the last use, for the idle unload.
    pub(crate) last_activity_ms: AtomicU64,
    /// Live file job leases on this engine.
    pub(crate) own_jobs: AtomicUsize,
    /// The state last emitted, so only changes are sent.
    pub(crate) last_event: Mutex<RecordingsEngineEvent>,
}

impl RecordingsEngine {
    /// Starts the supervisor's owner thread only.
    pub(crate) fn new(gpu_allowed: bool) -> Self {
        Self {
            engine: EngineSupervisor::new_with(gpu_allowed, WorkerOptions { low_priority: true }),
            model_id: Mutex::new(None),
            load_lock: Mutex::new(()),
            reload_on_next_use: AtomicBool::new(false),
            last_activity_ms: AtomicU64::new(now_ms()),
            own_jobs: AtomicUsize::new(0),
            last_event: Mutex::new(RecordingsEngineEvent::default()),
        }
    }

    /// Reset the idle timer to now.
    pub(crate) fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// How long the engine has been idle.
    pub(crate) fn idle(&self) -> Duration {
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        Duration::from_millis(now_ms().saturating_sub(last))
    }

    pub(crate) fn own_jobs(&self) -> usize {
        self.own_jobs.load(Ordering::SeqCst)
    }

    pub(crate) fn model_id(&self) -> Option<String> {
        lock(&self.model_id).clone()
    }

    pub(crate) fn set_model_id(&self, model_id: Option<String>) {
        *lock(&self.model_id) = model_id;
    }

    /// Wait for a recordings model load in progress, and keep others from
    /// starting while the guard lives.
    pub(crate) fn hold_loads(&self) -> MutexGuard<'_, ()> {
        lock(&self.load_lock)
    }

    /// The engine's state now: the loaded model, from the supervisor's
    /// snapshot (never waits on its queue).
    pub(crate) fn status(&self) -> RecordingsEngineEvent {
        match self.engine.loaded() {
            Some(info) => RecordingsEngineEvent {
                model_id: self.model_id(),
                on_gpu: info.on_gpu,
                gpu_memory_fallback: info.gpu_memory_fallback,
            },
            None => RecordingsEngineEvent::default(),
        }
    }

    /// The state now, if it differs from the one last taken; recorded as
    /// sent.
    pub(crate) fn take_changed_status(&self) -> Option<RecordingsEngineEvent> {
        let status = self.status();
        let mut last = lock(&self.last_event);
        if *last == status {
            return None;
        }
        *last = status.clone();
        Some(status)
    }
}

/// Lock, recovering from poison: every value guarded here is replaced
/// whole, so a panic while one was held leaves nothing half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slot_follows_the_setting() {
        let cases = [
            ("", "small", FileModelSlot::Dictation),
            ("  ", "small", FileModelSlot::Dictation),
            ("small", "small", FileModelSlot::Dictation),
            (" small ", "small", FileModelSlot::Dictation),
            ("cohere", "small", FileModelSlot::Own),
            // No dictation model: an explicit model still gets its own engine.
            ("x", "", FileModelSlot::Own),
            ("", "", FileModelSlot::Dictation),
        ];
        for (setting, selected, expected) in cases {
            assert_eq!(
                choose_file_model_slot(setting, selected),
                expected,
                "{setting:?} with {selected:?}"
            );
        }
    }

    #[test]
    fn the_linger_is_the_unload_timeout_up_to_two_minutes() {
        let cases = [
            (ModelUnloadTimeout::Immediately, 0),
            (ModelUnloadTimeout::Sec15, 15),
            (ModelUnloadTimeout::Min2, 120),
            (ModelUnloadTimeout::Min5, 120),
            (ModelUnloadTimeout::Min10, 120),
            (ModelUnloadTimeout::Hour1, 120),
            (ModelUnloadTimeout::Never, 120),
        ];
        for (timeout, secs) in cases {
            assert_eq!(
                recordings_linger(timeout),
                Duration::from_secs(secs),
                "{timeout:?}"
            );
        }
    }

    #[test]
    fn an_idle_unused_model_is_unloaded_after_its_linger() {
        let linger = Duration::from_secs(120);
        let long = Duration::from_secs(121);
        assert!(!should_unload_recordings(false, 0, long, linger));
        assert!(!should_unload_recordings(true, 1, long, linger));
        assert!(!should_unload_recordings(
            true,
            0,
            Duration::from_secs(119),
            linger
        ));
        assert!(should_unload_recordings(true, 0, linger, linger));
        assert!(should_unload_recordings(true, 0, long, linger));
        // "Immediately": any idle time will do.
        assert!(should_unload_recordings(
            true,
            0,
            Duration::ZERO,
            Duration::ZERO
        ));
    }

    #[test]
    fn vram_need_covers_both_models_with_headroom() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(vram_need(0, 0), 512 * MIB);
        assert_eq!(vram_need(1000 * MIB, 0), 1150 * MIB + 512 * MIB);
        assert_eq!(vram_need(600 * MIB, 400 * MIB), 1150 * MIB + 512 * MIB);
        assert_eq!(vram_need(100, 100), 230 + 512 * MIB);
        assert_eq!(vram_need(u64::MAX, 0), u64::MAX);
        assert_eq!(vram_need(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(vram_need(u64::MAX / 2, u64::MAX / 2), u64::MAX);
    }

    #[test]
    fn an_unloaded_engine_reports_nothing_and_changes_only_once() {
        let recordings = RecordingsEngine::new(false);
        assert_eq!(recordings.status(), RecordingsEngineEvent::default());
        // The default was "sent" at start: nothing new to emit.
        assert_eq!(recordings.take_changed_status(), None);
        // A model id without a loaded model reads as nothing loaded.
        recordings.set_model_id(Some("tiny".to_string()));
        assert_eq!(recordings.status(), RecordingsEngineEvent::default());
        assert_eq!(recordings.model_id().as_deref(), Some("tiny"));
        *lock(&recordings.last_event) = RecordingsEngineEvent {
            model_id: Some("tiny".to_string()),
            on_gpu: true,
            gpu_memory_fallback: false,
        };
        assert_eq!(
            recordings.take_changed_status(),
            Some(RecordingsEngineEvent::default())
        );
        assert_eq!(recordings.take_changed_status(), None);
    }

    #[test]
    fn idle_time_counts_from_the_last_touch() {
        let recordings = RecordingsEngine::new(false);
        recordings
            .last_activity_ms
            .store(now_ms() - 5_000, Ordering::Relaxed);
        assert!(recordings.idle() >= Duration::from_secs(5));
        recordings.touch();
        assert!(recordings.idle() < Duration::from_secs(5));
        assert_eq!(recordings.own_jobs(), 0);
    }
}
