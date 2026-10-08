use crate::audio_toolkit::{
    apply_custom_words, detect_output_language, normalize_transcription_output,
    remove_filler_words, OutputLanguageEvidence,
};
use crate::chinese_script::{convert_chinese_script, ChineseVariety};
use crate::engine_supervisor::{
    DeviceInfo, DeviceSelector, EngineError, EngineSupervisor, LoadSpec, LoadedInfo,
    StreamProgress, Unloading, WorkToken,
};
use crate::file_transcription::types::{TimingGrain, TranscriptSegment};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::model::{EngineType, ModelManager};
use crate::settings::{
    get_settings, AppSettings, ChineseScript, ModelUnloadTimeout, OrtAcceleratorSetting,
    TranscribeAcceleratorSetting,
};
use anyhow::Result;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{fence, AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tauri::{AppHandle, Emitter, Manager};
use tauri_specta::Event;
use transcribe_cpp::{
    Backend, RunExtension, RunOptions, StreamOptions, Task, TimestampKind, WhisperRunOptions,
};
use transcribe_rs::{
    onnx::{
        canary::CanaryModel,
        cohere::CohereModel,
        gigaam::GigaAMModel,
        moonshine::{MoonshineModel, MoonshineVariant, StreamingModel},
        parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity},
        sense_voice::{SenseVoiceModel, SenseVoiceParams},
        Quantization,
    },
    SpeechModel, TranscribeOptions,
};

const STREAM_PERF_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// How often a file job waiting for a model load checks its cancel token.
const FILE_JOB_LOAD_POLL: Duration = Duration::from_millis(200);
/// How often a file job that must replace the loaded model checks whether
/// the dictation using it has finished.
const FILE_JOB_DICTATION_POLL: Duration = Duration::from_millis(150);
/// Times one file chunk is retried after a global cancel (Escape during a
/// dictation) stopped it.
const FILE_CHUNK_FOREIGN_CANCEL_RETRIES: u32 = 2;
/// Times the job's model is reloaded for one file chunk after it was
/// unloaded or its engine was lost.
const FILE_CHUNK_RELOADS: u32 = 2;
/// Length given to a timed segment the engine reported with no duration.
const MIN_SEGMENT_MS: u64 = 500;

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelStateEvent {
    pub event_type: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub error: Option<String>,
}

/// Live transcription snapshot emitted to the overlay during a streaming run.
/// `committed` is the append-only, flicker-free prefix; `tentative` is the
/// volatile suffix the model may still rewrite.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct StreamTextEvent {
    pub committed: String,
    pub tentative: String,
}

/// Phase of the streaming overlay card, emitted to drive its UI state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum StreamPhase {
    /// Receiving audio / live text (or waiting for the stream to begin). Rust
    /// does not emit this today; the frontend starts in this phase and Rust only
    /// emits transitions away from it.
    Listening,
    /// Finalizing or post-processing — show a spinner.
    Working,
}

/// Semantic kind of "working" phase, used to localize the spinner label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum StreamWorkKind {
    Transcribing,
    Polishing,
}

/// Emitted to switch the streaming overlay to a working spinner.
#[derive(Clone, Debug, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct StreamPhaseEvent {
    pub phase: StreamPhase,
    /// Present only when `phase` is `Working`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<StreamWorkKind>,
}

/// Commands sent to the streaming worker thread. Audio frames and the finalize
/// request travel the same channel so FIFO ordering guarantees every fed frame
/// is processed before finalize runs.
enum StreamCmd {
    Feed(Vec<f32>),
    /// Flush the stream and reply with the final text, or `None` if no
    /// usable stream exists (caller should fall back to batch transcription).
    /// `Err(Cancelled)` if the finalize itself was cancelled.
    Finalize(mpsc::Sender<StreamFinalizeReply>),
    Cancel,
}

type StreamFinalizeReply = Result<Option<FinalizedStreamText>, EngineError>;

struct FinalizedStreamText {
    text: String,
    output_language: OutputLanguageEvidence,
    /// The streaming model's supported languages, for text-based detection.
    supported_languages: Vec<String>,
}

/// Routes real-time audio frames to the active streaming worker. Shared between
/// the [`TranscriptionManager`] (opens/closes the route) and the audio recorder's
/// per-frame callback (feeds frames). The recorder holds an `Arc<StreamRouter>`
/// directly, so a frame with no stream pending costs a single relaxed atomic
/// load — no Tauri state lookup, no mutex lock.
pub struct StreamRouter {
    /// Command channel to the active streaming worker, present from
    /// `start_stream` until `finalize_stream`/`cancel_stream`.
    tx: Mutex<Option<mpsc::Sender<StreamCmd>>>,
    /// True while a stream is pending or active (channel is open). The audio
    /// callback checks this first to avoid the mutex lock when no stream runs.
    open: Arc<AtomicBool>,
}

impl StreamRouter {
    fn new() -> Self {
        Self {
            tx: Mutex::new(None),
            open: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Open a fresh command channel for a new streaming session, returning the
    /// receiver the worker should drain. Caller must ensure no prior channel is
    /// still open.
    fn open(&self) -> mpsc::Receiver<StreamCmd> {
        let (tx, rx) = mpsc::channel::<StreamCmd>();
        *self.tx.lock().unwrap() = Some(tx);
        self.open.store(true, Ordering::Relaxed);
        rx
    }

    /// Take the sender out (closing the channel to new feeds). Returns the
    /// sender so the caller can send the final `Finalize`/`Cancel` command.
    fn take(&self) -> Option<mpsc::Sender<StreamCmd>> {
        self.open.store(false, Ordering::Relaxed);
        self.tx.lock().unwrap().take()
    }

    /// Drop the channel and mark closed without sending a final command (used
    /// when the worker exits without a finalize/cancel handshake).
    fn clear(&self) {
        self.open.store(false, Ordering::Relaxed);
        *self.tx.lock().unwrap() = None;
    }

    /// Forward a 16 kHz frame to the active streaming worker. Cheap no-op (a
    /// single relaxed atomic load) when no stream is pending.
    pub fn feed(&self, frame: &[f32]) {
        if !self.open.load(Ordering::Relaxed) {
            return;
        }
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(StreamCmd::Feed(frame.to_vec()));
        }
    }

    /// Whether a stream is pending or active.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }
}

/// In-process ONNX engines. transcribe-cpp models (whisper family, parakeet,
/// custom .bin/.gguf) live in the [`EngineSupervisor`]'s worker process
/// instead, so a native crash or hang there cannot take the app down.
enum OnnxEngine {
    Parakeet(ParakeetModel),
    Moonshine(MoonshineModel),
    MoonshineStreaming(StreamingModel),
    SenseVoice(SenseVoiceModel),
    GigaAM(GigaAMModel),
    Canary(CanaryModel),
    Cohere(CohereModel),
}

/// RAII guard that clears the `is_loading` flag and notifies waiters on drop.
/// Ensures the loading flag is always reset, even on early returns or panics.
pub struct LoadingGuard {
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        // Recover from a poisoned mutex instead of panicking —
        // a panic inside Drop calls abort().
        let mut is_loading = match self.is_loading.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("Recovered poisoned is_loading mutex during LoadingGuard drop — a panic occurred earlier this session");
                e.into_inner()
            }
        };
        *is_loading = false;
        self.loading_condvar.notify_all();
    }
}

/// Wait until no model load is in progress; then, unless `loaded()`, mark a
/// load as started and return its guard, which the caller holds while it
/// loads. Checked and claimed under one lock, so no other load can start in
/// between. `None` means the model is loaded and the flag was not touched.
fn claim_load_unless_loaded(
    is_loading: &Arc<Mutex<bool>>,
    loading_condvar: &Arc<Condvar>,
    loaded: impl FnOnce() -> bool,
) -> Option<LoadingGuard> {
    let mut flag = is_loading.lock().unwrap();
    while *flag {
        flag = loading_condvar.wait(flag).unwrap();
    }
    if loaded() {
        return None;
    }
    *flag = true;
    Some(LoadingGuard {
        is_loading: Arc::clone(is_loading),
        loading_condvar: Arc::clone(loading_condvar),
    })
}

/// RAII guard that clears [`TranscriptionManager::onnx_in_use`] on every exit
/// from an ONNX run, including an unwinding panic.
struct OnnxInUse(Arc<AtomicBool>);

impl Drop for OnnxInUse {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// RAII guard that clears the streaming worker flags on any worker exit -
/// normal return, early return, or a panic that unwinds the detached worker
/// thread. Tokens prevent an older worker from clearing a newer worker's
/// state if a start/finalize race ever slips through.
struct StreamWorkerGuard {
    worker_id: u64,
    active_stream_worker: Arc<AtomicU64>,
    stream_active: Arc<AtomicBool>,
}

impl Drop for StreamWorkerGuard {
    fn drop(&mut self) {
        if self.active_stream_worker.load(Ordering::Acquire) == self.worker_id {
            self.stream_active.store(false, Ordering::Release);
        }
        let _ = self.active_stream_worker.compare_exchange(
            self.worker_id,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

#[derive(Clone)]
pub struct TranscriptionManager {
    /// The transcribe-cpp engine: owns the worker process, its model, and
    /// recovery from crashes and hangs.
    engine: EngineSupervisor,
    /// The loaded ONNX engine, if the current model is an ONNX one.
    onnx: Arc<Mutex<Option<OnnxEngine>>>,
    model_manager: Arc<ModelManager>,
    app_handle: AppHandle,
    current_model_id: Arc<Mutex<Option<String>>>,
    last_activity: Arc<AtomicU64>,
    shutdown_signal: Arc<AtomicBool>,
    watcher_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    is_loading: Arc<Mutex<bool>>,
    loading_condvar: Arc<Condvar>,
    reload_model_on_next_use: Arc<AtomicBool>,
    /// Routes real-time audio frames to the active streaming worker; see
    /// [`StreamRouter`]. Shared with the audio recorder so per-frame feeds skip
    /// Tauri state and the manager lock.
    router: Arc<StreamRouter>,
    /// True only while a transcribe-cpp stream is actually in flight (set by
    /// the worker once the stream begins). Used for overlay/UI decisions.
    stream_active: Arc<AtomicBool>,
    /// Streaming uses three independent flags: router open = frames should
    /// route, worker active = no second worker may start, stream active = UI
    /// should show a live session.
    ///
    /// Monotonic id source for stream workers; zero means "no worker".
    next_stream_worker_id: Arc<AtomicU64>,
    /// Nonzero while a stream worker exists, even if it has not leased the engine
    /// yet. This prevents a second worker from starting after finalize/cancel
    /// closes the router but before the first worker has fully exited.
    active_stream_worker: Arc<AtomicU64>,
    /// Held across every engine call: batch dictation, retry, file chunks,
    /// and a live stream's begin (only its begin). Engine users never
    /// interleave, so an ONNX engine is taken and returned by one caller at a
    /// time, and a file chunk can never be queued after a stream began. A
    /// leaf lock: never held while waiting on `is_loading`, loading a model,
    /// finalizing a stream or feeding one.
    run_gate: Arc<Mutex<()>>,
    /// True while an ONNX engine is taken out of `onnx` for a run; the model
    /// still counts as loaded then (see [`Self::is_model_loaded`]).
    onnx_in_use: Arc<AtomicBool>,
    /// Live [`FileJobLease`]s. While nonzero the model is never unloaded by
    /// "Immediately" or the idle watcher.
    active_file_jobs: Arc<AtomicUsize>,
    /// Outstanding [`DictationHold`]s: dictations past recording whose
    /// pipeline has not finished yet.
    dictation_holds: Arc<AtomicUsize>,
    /// Set when an immediate unload was deferred for a file job, so the
    /// dictation that asked for it can still unload once the job ended (see
    /// [`Self::release_dictation_hold`]).
    unload_deferred: Arc<AtomicBool>,
    /// Bumped by [`Self::cancel_transcription`], so a dictation still waiting
    /// for the run gate when the user cancels never starts its run.
    transcription_cancels: Arc<AtomicU64>,
}

impl TranscriptionManager {
    pub fn new(app_handle: &AppHandle, model_manager: Arc<ModelManager>) -> Result<Self> {
        let manager = Self {
            engine: EngineSupervisor::new(!transcribe_gpu_disabled_for_host()),
            onnx: Arc::new(Mutex::new(None)),
            model_manager,
            app_handle: app_handle.clone(),
            current_model_id: Arc::new(Mutex::new(None)),
            last_activity: Arc::new(AtomicU64::new(Self::now_ms())),
            shutdown_signal: Arc::new(AtomicBool::new(false)),
            watcher_handle: Arc::new(Mutex::new(None)),
            is_loading: Arc::new(Mutex::new(false)),
            loading_condvar: Arc::new(Condvar::new()),
            reload_model_on_next_use: Arc::new(AtomicBool::new(false)),
            router: Arc::new(StreamRouter::new()),
            stream_active: Arc::new(AtomicBool::new(false)),
            next_stream_worker_id: Arc::new(AtomicU64::new(1)),
            active_stream_worker: Arc::new(AtomicU64::new(0)),
            run_gate: Arc::new(Mutex::new(())),
            onnx_in_use: Arc::new(AtomicBool::new(false)),
            active_file_jobs: Arc::new(AtomicUsize::new(0)),
            dictation_holds: Arc::new(AtomicUsize::new(0)),
            unload_deferred: Arc::new(AtomicBool::new(false)),
            transcription_cancels: Arc::new(AtomicU64::new(0)),
        };

        // Start the idle watcher
        {
            let app_handle_cloned = app_handle.clone();
            let manager_cloned = manager.clone();
            let shutdown_signal = manager.shutdown_signal.clone();
            let handle = thread::spawn(move || {
                debug!("Idle watcher thread started");
                while !shutdown_signal.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(10)); // Check every 10 seconds

                    // Check shutdown signal again after sleep
                    if shutdown_signal.load(Ordering::Relaxed) {
                        break;
                    }

                    let settings = get_settings(&app_handle_cloned);
                    let timeout = settings.model_unload_timeout;

                    // Skip Immediately — that variant is handled by
                    // maybe_unload_immediately() after each transcription.
                    // Treating it as 0s here would unload the model mid-recording.
                    if timeout == ModelUnloadTimeout::Immediately {
                        continue;
                    }

                    // While recording, keep the idle timer fresh so the
                    // model is never unloaded mid-session.
                    let is_recording = app_handle_cloned
                        .try_state::<Arc<AudioRecordingManager>>()
                        .is_some_and(|a| a.is_recording());
                    if is_recording {
                        manager_cloned.touch_activity();
                        continue;
                    }

                    // Likewise while a file is being transcribed: its chunks
                    // run back to back, and a dictation may pause it.
                    if manager_cloned.active_file_jobs.load(Ordering::Acquire) > 0 {
                        manager_cloned.touch_activity();
                        continue;
                    }

                    if let Some(limit_seconds) = timeout.to_seconds() {
                        let last = manager_cloned.last_activity.load(Ordering::Relaxed);
                        let now_ms = TranscriptionManager::now_ms();
                        let idle_ms = now_ms.saturating_sub(last);
                        let limit_ms = limit_seconds * 1000;

                        if idle_ms > limit_ms {
                            // idle -> unload
                            if manager_cloned.is_model_loaded() {
                                let unload_start = std::time::Instant::now();
                                info!(
                                    "Model idle for {}s (limit: {}s), unloading",
                                    idle_ms / 1000,
                                    limit_seconds
                                );
                                match manager_cloned.unload_model() {
                                    Ok(()) => {
                                        let unload_duration = unload_start.elapsed();
                                        info!(
                                            "Model unloaded due to inactivity (took {}ms)",
                                            unload_duration.as_millis()
                                        );
                                    }
                                    Err(e) => {
                                        error!("Failed to unload idle model: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
                debug!("Idle watcher thread shutting down gracefully");
            });
            *manager.watcher_handle.lock().unwrap() = Some(handle);
        }

        Ok(manager)
    }

    /// Lock the ONNX engine mutex, recovering from poison if a previous transcription panicked.
    fn lock_onnx(&self) -> MutexGuard<'_, Option<OnnxEngine>> {
        self.onnx.lock().unwrap_or_else(|poisoned| {
            warn!("Engine mutex was poisoned by a previous panic, recovering");
            poisoned.into_inner()
        })
    }

    /// Lock the run gate (see [`Self::run_gate`]), recovering from poison:
    /// the gate guards no data, so a panic while it was held leaves nothing
    /// inconsistent.
    fn lock_run_gate(&self) -> MutexGuard<'_, ()> {
        self.run_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn is_model_loaded(&self) -> bool {
        // A transcribe-cpp model stays loaded while it is busy, so a batch
        // run or stream in progress never reads as "unloaded". Likewise an
        // ONNX engine taken out for a run: a dictation must neither load a
        // second copy of it nor fail, but wait at the run gate for it.
        if self.engine.loaded().is_some() || self.lock_onnx().is_some() {
            return true;
        }
        // Checked after the ONNX lock is released: never hold both.
        self.onnx_in_use.load(Ordering::Acquire) && self.get_current_model().is_some()
    }

    /// Accelerator changes should not disturb the current transcription. Mark
    /// the cached engine stale; the next model-use path reloads it with the
    /// latest settings.
    pub fn reload_model_on_next_use(&self) {
        self.reload_model_on_next_use.store(true, Ordering::Release);
    }

    /// An explicit transcribe.cpp accelerator or GPU device change is the
    /// user's way of trying the GPU again: forget every GPU failure so far,
    /// so the next load may use (and list) the GPU. The ONNX accelerator does
    /// not affect this.
    pub fn retry_transcribe_gpu(&self, reason: &str) {
        self.engine.retry_gpu(reason);
    }

    /// The engine gives up on a model by itself when its worker can't be
    /// restarted (e.g. the file is gone, or it crashes on CPU too). Keep the
    /// loaded model id and the UI in step, so the next use loads the model
    /// again and reports why it can't.
    fn forget_model_if_engine_dropped(&self, model_id: &str, error: &str) {
        if self.engine.loaded().is_some() {
            return;
        }
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            if current_model.as_deref() != Some(model_id) {
                return;
            }
            *current_model = None;
        }
        warn!(
            "Transcription model '{}' is no longer loaded: {}",
            model_id, error
        );
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: Some(error.to_string()),
            },
        );
    }

    /// Atomically check whether a model load is in progress and, if not, mark
    /// one as starting. Returns a [`LoadingGuard`] whose [`Drop`] impl will
    /// clear the flag and wake waiters. Returns `None` if a load is already in
    /// progress.
    pub fn try_start_loading(&self) -> Option<LoadingGuard> {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading {
            return None;
        }
        *is_loading = true;
        Some(LoadingGuard {
            is_loading: self.is_loading.clone(),
            loading_condvar: self.loading_condvar.clone(),
        })
    }

    /// Unload the model. Returns once the transcribe-cpp worker has exited,
    /// which frees all of its memory (e.g. before its file is deleted).
    pub fn unload_model(&self) -> Result<()> {
        let unload_start = std::time::Instant::now();
        debug!("Starting to unload model");
        self.begin_unload().wait();
        debug!(
            "Model unloaded manually (took {}ms)",
            unload_start.elapsed().as_millis()
        );
        Ok(())
    }

    /// Unload the model without waiting for the transcribe-cpp worker to
    /// exit, so it never blocks the caller (e.g. the event loop) behind a
    /// transcription in progress. The model reads as unloaded at once.
    pub fn request_unload(&self) {
        drop(self.begin_unload());
    }

    /// Every state change of an unload, done now on the caller's thread: a
    /// dictation started right after sees no model and loads it again, and
    /// its load is queued behind this unload, so the unload can never affect
    /// it. Only the worker's exit is left to wait for.
    fn begin_unload(&self) -> Unloading {
        let unloading = self.engine.unload();
        // Dropping an ONNX engine frees its resources.
        *self.lock_onnx() = None;
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = None;
        }

        // Emit unloaded event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "unloaded".to_string(),
                model_id: None,
                model_name: None,
                error: None,
            },
        );
        unloading
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Reset the idle timer to now.
    fn touch_activity(&self) {
        self.last_activity.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Unloads the model immediately if the setting is enabled and the model
    /// is loaded. Never waits for the worker to exit, so it is safe on any
    /// thread, including the event loop.
    ///
    /// Deferred while a file is being transcribed: the job's lease unloads
    /// the model when it ends (see [`FileJobLease`]).
    pub fn maybe_unload_immediately(&self, context: &str) {
        if self.active_file_jobs.load(Ordering::SeqCst) > 0 {
            self.unload_deferred.store(true, Ordering::SeqCst);
            debug!(
                "Deferring immediate unload ({}): file transcription running",
                context
            );
            return;
        }
        self.unload_deferred.store(false, Ordering::SeqCst);
        let settings = get_settings(&self.app_handle);
        if settings.model_unload_timeout == ModelUnloadTimeout::Immediately
            && self.is_model_loaded()
        {
            info!("Immediately unloading model after {}", context);
            self.request_unload();
        }
    }

    pub fn load_model(&self, model_id: &str) -> Result<()> {
        self.load_model_with_device(model_id, None)
    }

    /// Like [`load_model`](Self::load_model), but lets a caller hard-select the
    /// compute device for this one load by its transcribe-cpp device
    /// registry index (the index shown by `--list-devices`). `None` keeps the
    /// persisted accelerator setting (which may be Auto). Only affects
    /// transcribe-cpp (whisper-family) models; the selection is not persisted.
    pub fn load_model_with_device(
        &self,
        model_id: &str,
        device_index: Option<usize>,
    ) -> Result<()> {
        apply_accelerator_settings(&self.app_handle);

        let load_start = std::time::Instant::now();
        debug!("Starting to load model: {}", model_id);

        // Emit loading started event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_started".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: None,
                error: None,
            },
        );

        let model_info = match self.model_manager.get_model_info(model_id) {
            Some(model_info) => model_info,
            None => {
                let error_msg = format!("Model not found: {}", model_id);
                let _ = self.app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "loading_failed".to_string(),
                        model_id: Some(model_id.to_string()),
                        model_name: None,
                        error: Some(error_msg.clone()),
                    },
                );
                return Err(anyhow::anyhow!(error_msg));
            }
        };

        // Every failure after loading starts must emit a terminal event so the
        // frontend can never remain in its loading state.
        let emit_loading_failed = |error_msg: &str| {
            let _ = self.app_handle.emit(
                "model-state-changed",
                ModelStateEvent {
                    event_type: "loading_failed".to_string(),
                    model_id: Some(model_id.to_string()),
                    model_name: Some(model_info.name.clone()),
                    error: Some(error_msg.to_string()),
                },
            );
        };

        if !model_info.is_downloaded {
            let error_msg = "Model not downloaded";
            emit_loading_failed(error_msg);
            return Err(anyhow::anyhow!(error_msg));
        }

        let model_path = self
            .model_manager
            .get_model_path(model_id)
            .inspect_err(|error| emit_loading_failed(&error.to_string()))?;

        // Drop the current engine BEFORE building the new one so the previous
        // model is freed first — avoids holding two models at once (peak memory
        // on large GGUFs). A transcribe-cpp load replaces its worker the same
        // way. Clear the id too: if the new load fails, status should read "no
        // loaded model", not the dropped engine.
        *self.lock_onnx() = None;
        if !matches!(model_info.engine_type, EngineType::TranscribeCpp) {
            self.engine.unload().wait();
        }
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = None;
        }

        // Create appropriate engine based on model type

        let loaded_onnx = match model_info.engine_type {
            EngineType::TranscribeCpp => {
                // The backend is chosen at load time. With an explicit
                // `device_index` (the --device-index flag) hard-select that
                // registered device; otherwise re-read the persisted
                // accelerator preference (so an accelerator change marked for
                // reload takes effect here). The worker resolves the selector
                // to a device handle, and rejects an index that isn't a
                // loadable device (a host without GPU access lists none).
                let (backend, device) = match device_index {
                    Some(index) => (Backend::Auto, DeviceSelector::Index(index)),
                    None => {
                        let settings = get_settings(&self.app_handle);
                        let accelerator = settings.transcribe_accelerator;
                        match resolve_gpu_device(
                            accelerator,
                            settings.transcribe_gpu_device.as_deref(),
                        ) {
                            // Backend::Auto accepts an exact GPU device.
                            Some(key) => (Backend::Auto, DeviceSelector::Key(key)),
                            // Without a valid exact device, backend selection
                            // handles the retired generic GPU state and host
                            // CPU guard.
                            None => (select_transcribe_backend(accelerator), DeviceSelector::Auto),
                        }
                    }
                };
                let requested_device = format!("{:?}", device);
                let info = self
                    .engine
                    .load(LoadSpec {
                        path: model_path.clone(),
                        backend,
                        device,
                    })
                    .map_err(|e| {
                        let error_msg = format!("Failed to load model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                // Reconcile the registry's advertised capabilities with the
                // loaded model's real ones (GGUF metadata) so badges/gating
                // reflect runtime truth, not the pre-download probe. The
                // load-completed event below triggers the frontend refresh.
                let caps = &info.capabilities;
                self.model_manager.set_runtime_capabilities(
                    model_id,
                    caps.supports_streaming,
                    caps.supports_translate,
                    caps.supports_language_detect,
                    caps.languages.clone(),
                );
                // The bound backend may differ from the request (e.g. CPU
                // fallback under Auto or after a GPU crash); log what loaded.
                info!(
                    "Loaded transcribe-cpp model '{}' (requested {:?}, requested device {}, \
                     bound backend '{}', bound device '{}', supports_streaming={}, \
                     supports_translate={}, supports_language_detect={})",
                    model_id,
                    backend,
                    requested_device,
                    info.backend,
                    info.device,
                    caps.supports_streaming,
                    caps.supports_translate,
                    caps.supports_language_detect
                );
                None
            }
            EngineType::Parakeet => {
                let engine =
                    ParakeetModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load parakeet model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                Some(OnnxEngine::Parakeet(engine))
            }
            EngineType::Moonshine => {
                let engine = MoonshineModel::load(
                    &model_path,
                    MoonshineVariant::Base,
                    &Quantization::default(),
                )
                .map_err(|e| {
                    let error_msg = format!("Failed to load moonshine model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::Moonshine(engine))
            }
            EngineType::MoonshineStreaming => {
                let engine = StreamingModel::load(&model_path, 0, &Quantization::default())
                    .map_err(|e| {
                        let error_msg = format!(
                            "Failed to load moonshine streaming model {}: {}",
                            model_id, e
                        );
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                Some(OnnxEngine::MoonshineStreaming(engine))
            }
            EngineType::SenseVoice => {
                let engine =
                    SenseVoiceModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                        let error_msg =
                            format!("Failed to load SenseVoice model {}: {}", model_id, e);
                        emit_loading_failed(&error_msg);
                        anyhow::anyhow!(error_msg)
                    })?;
                Some(OnnxEngine::SenseVoice(engine))
            }
            EngineType::GigaAM => {
                let engine = GigaAMModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load gigaam model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::GigaAM(engine))
            }
            EngineType::Canary => {
                let engine = CanaryModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load canary model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::Canary(engine))
            }
            EngineType::Cohere => {
                let engine = CohereModel::load(&model_path, &Quantization::Int8).map_err(|e| {
                    let error_msg = format!("Failed to load cohere model {}: {}", model_id, e);
                    emit_loading_failed(&error_msg);
                    anyhow::anyhow!(error_msg)
                })?;
                Some(OnnxEngine::Cohere(engine))
            }
        };

        // Update the current engine and model ID
        *self.lock_onnx() = loaded_onnx;
        {
            let mut current_model = self.current_model_id.lock().unwrap();
            *current_model = Some(model_id.to_string());
        }

        // Reset idle timer so the watcher doesn't immediately unload a just-loaded model
        self.touch_activity();

        // Emit loading completed event
        let _ = self.app_handle.emit(
            "model-state-changed",
            ModelStateEvent {
                event_type: "loading_completed".to_string(),
                model_id: Some(model_id.to_string()),
                model_name: Some(model_info.name.clone()),
                error: None,
            },
        );

        let load_duration = load_start.elapsed();
        debug!(
            "Successfully loaded transcription model: {} (took {}ms)",
            model_id,
            load_duration.as_millis()
        );
        Ok(())
    }

    /// Kicks off the model loading in a background thread if it's not already loaded
    pub fn initiate_model_load(&self) {
        let mut is_loading = self.is_loading.lock().unwrap();
        if *is_loading {
            return;
        }

        let reload_pending = self.reload_model_on_next_use.load(Ordering::Acquire);
        if !reload_pending && self.is_model_loaded() {
            return;
        }

        *is_loading = true;
        let self_clone = self.clone();
        thread::spawn(move || {
            // Clears the flag and wakes waiters even if the load panics, so
            // no waiter (a dictation or a file job) can hang on it.
            let _loading = LoadingGuard {
                is_loading: self_clone.is_loading.clone(),
                loading_condvar: self_clone.loading_condvar.clone(),
            };
            if reload_pending {
                self_clone
                    .reload_model_on_next_use
                    .store(false, Ordering::Release);
            }
            let settings = get_settings(&self_clone.app_handle);
            if let Err(e) = self_clone.load_model(&settings.selected_model) {
                error!("Failed to load model: {}", e);
            }
        });
    }

    pub fn get_current_model(&self) -> Option<String> {
        let current_model = self.current_model_id.lock().unwrap();
        current_model.clone()
    }

    /// The compute backend the currently-loaded engine is bound to, for
    /// diagnostics (e.g. confirming `--device-index` actually bound a GPU rather
    /// than falling back to CPU/auto). transcribe-cpp (whisper-family) reports
    /// its real backend string; ONNX engines report "onnx"; `None` when no
    /// model is loaded.
    pub fn current_backend(&self) -> Option<String> {
        if let Some(info) = self.engine.loaded() {
            return Some(info.backend);
        }
        self.lock_onnx().as_ref().map(|_| "onnx".to_string())
    }

    /// Whether a live streaming run is currently in flight.
    pub fn is_streaming(&self) -> bool {
        self.stream_active.load(Ordering::Acquire)
    }

    /// Shared handle to the stream router, used by the audio recorder to feed
    /// real-time frames without going through Tauri state on every frame.
    pub fn stream_router(&self) -> Arc<StreamRouter> {
        Arc::clone(&self.router)
    }

    /// Begin a live streaming transcription on the held engine's session.
    /// Audio frames pushed via [`StreamRouter::feed`] (captured directly by the
    /// audio recorder) are decoded incrementally and emitted to the overlay as
    /// [`StreamTextEvent`].
    ///
    /// Non-blocking: spawns a worker that waits for any in-progress model load,
    /// verifies the model supports streaming, then begins the stream. If the
    /// model can't stream, the worker idles until finalize/cancel and reports
    /// `None` so the caller falls back to batch transcription. Frames sent
    /// before the stream begins queue on the channel and are not lost.
    pub fn start_stream(&self) {
        if self.router.is_open() || self.active_stream_worker.load(Ordering::Acquire) != 0 {
            warn!("start_stream called while a stream worker is already active");
            return;
        }
        let worker_id = self.next_stream_worker_id.fetch_add(1, Ordering::Relaxed);
        if self
            .active_stream_worker
            .compare_exchange(0, worker_id, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            warn!("start_stream lost a race with another stream worker");
            return;
        }
        let rx = self.router.open();
        self.stream_active.store(false, Ordering::Release);

        let manager = self.clone();
        thread::spawn(move || manager.run_stream_worker(rx, worker_id));
    }

    fn run_stream_worker(&self, rx: mpsc::Receiver<StreamCmd>, worker_id: u64) {
        let _worker = StreamWorkerGuard {
            worker_id,
            active_stream_worker: Arc::clone(&self.active_stream_worker),
            stream_active: Arc::clone(&self.stream_active),
        };

        // Wait for any in-progress model load to finish (start_stream races the
        // background load kicked off when recording starts).
        {
            let mut is_loading = self.is_loading.lock().unwrap();
            while *is_loading {
                is_loading = self.loading_condvar.wait(is_loading).unwrap();
            }
        }

        let model_id = self.get_current_model().unwrap_or_default();

        // Only transcribe-cpp models expose streaming; ONNX engines fall back to
        // batch. The loaded model (not the ModelManager copy) is the source of
        // truth for run-path capabilities.
        let Some(info) = self.engine.loaded() else {
            info!(
                "Live preview: model '{}' is not a loaded transcribe-cpp model; \
                 streaming is unavailable, using batch transcription",
                model_id
            );
            self.router.clear();
            drain_until_finalize(rx);
            return;
        };
        let caps = &info.capabilities;
        info!(
            "Live preview: model '{}' arch='{}' variant='{}' supports_streaming={} \
             supports_translate={} languages={:?}",
            model_id,
            info.arch,
            info.variant,
            caps.supports_streaming,
            caps.supports_translate,
            caps.languages,
        );
        if !caps.supports_streaming {
            self.router.clear();
            drain_until_finalize(rx);
            return;
        }
        let languages = caps.languages.clone();

        // Build run options mirroring the offline transcribe-cpp path: task +
        // language gated against what the model actually advertises.
        let settings = get_settings(&self.app_handle);
        let effective_language =
            effective_language_for_model(&settings, self.model_manager.as_ref(), &model_id);
        let run_plan = transcribe_cpp_run_plan(
            settings.translate_to_english,
            &effective_language,
            &languages,
            caps.supports_translate,
        );
        let output_language = resolve_output_language_evidence(
            &settings,
            run_plan.language.as_deref(),
            &languages,
            run_plan.target_language.as_deref() == Some("en"),
        );
        let run_options = RunOptions {
            task: run_plan.task,
            language: run_plan.language,
            target_language: run_plan.target_language,
            ..Default::default()
        };

        // Feed results arrive on the engine's thread; this callback only
        // converts and emits preview text.
        let perf = Arc::new(Mutex::new(StreamPerf::new()));
        let on_progress = {
            let perf = Arc::clone(&perf);
            let app_handle = self.app_handle.clone();
            let languages = languages.clone();
            let mut preview_script = PreviewScript::new(settings.chinese_script, &output_language);
            move |progress: StreamProgress| {
                let mut perf = lock_perf(&perf);
                perf.record_compute(progress.elapsed);
                perf.record_update(&progress.update);
                if let Some(text) = progress.text {
                    perf.record_emit();
                    let (committed, tentative) =
                        preview_script.convert(&text.committed, &text.tentative, &languages);
                    emit_stream_text(&app_handle, &committed, &tentative);
                }
                perf.maybe_log();
            }
        };

        // Run the stream in the engine's worker process. Feeds are queued
        // without waiting; a crashed or hung worker makes finalize report no
        // result, and the caller falls back to batch transcription. StreamOptions::default()
        // uses CommitPolicy::Auto and lets the family pick its own streaming
        // strategy (no family-specific ext). Only the begin runs under the run
        // gate (so it waits for a file chunk in flight); the gate is released
        // before any feed or finalize, so file jobs never block a dictation's
        // stream, and they don't run while it is open (`dictation_active`).
        let started = {
            let _gate = self.lock_run_gate();
            self.engine
                .start_stream(run_options, StreamOptions::default(), on_progress)
        };
        let stream = match started {
            Ok(stream) => stream,
            Err(e) => {
                error!("Failed to begin stream: {}", e);
                self.forget_model_if_engine_dropped(&model_id, &e.to_string());
                drain_until_finalize(rx);
                return;
            }
        };

        self.stream_active.store(true, Ordering::Release);
        self.touch_activity();
        info!(
            "Live streaming transcription started (model '{}', backend '{}')",
            model_id, info.backend
        );

        while let Ok(cmd) = rx.recv() {
            match cmd {
                StreamCmd::Feed(pcm) => {
                    self.touch_activity();
                    lock_perf(&perf).record_feed(pcm.len());
                    stream.feed(pcm);
                }
                StreamCmd::Finalize(reply) => {
                    // In auto mode the model's own LID is the best remaining
                    // evidence; the snapshot is only materialized when it can
                    // change the outcome.
                    let want_language = matches!(output_language, OutputLanguageEvidence::Unknown);
                    let result = match stream.finalize(want_language) {
                        // After finalize the committed prefix holds the full
                        // text; display() = committed + tentative is the safe read.
                        Ok(Some(finalized)) => {
                            let mut perf = lock_perf(&perf);
                            perf.record_compute(finalized.elapsed);
                            perf.record_update(&finalized.update);
                            let output_language = match &output_language {
                                OutputLanguageEvidence::Unknown => with_model_detected_language(
                                    OutputLanguageEvidence::Unknown,
                                    finalized.language,
                                ),
                                resolved => resolved.clone(),
                            };
                            Ok(Some(FinalizedStreamText {
                                text: finalized.text.full,
                                output_language,
                                supported_languages: languages.clone(),
                            }))
                        }
                        Ok(None) => {
                            error!(
                                "Live transcription could not be finalized; \
                                 falling back to batch transcription"
                            );
                            self.forget_model_if_engine_dropped(
                                &model_id,
                                "live transcription failed and the model could not be restarted",
                            );
                            Ok(None)
                        }
                        Err(e) => {
                            info!("Live transcription finalize stopped: {}", e);
                            Err(e)
                        }
                    };
                    let chars = match &result {
                        Ok(Some(finalized)) => finalized.text.len(),
                        _ => 0,
                    };
                    lock_perf(&perf).log_finalized(chars);
                    let _ = reply.send(result);
                    return;
                }
                // Dropping the stream handle resets the worker's stream.
                StreamCmd::Cancel => return,
            }
        }
        // The channel closed without finalize/cancel; dropping the stream
        // handle resets the worker's stream. `_worker` drops after, clearing
        // this worker's flags.
    }

    /// Return the taken ONNX engine to the mutex, unless the model was switched
    /// or unloaded during transcription (in which case the stale engine is dropped).
    fn return_engine(&self, engine: OnnxEngine, expected_model_id: &str) {
        let still_current =
            self.current_model_id.lock().unwrap().as_deref() == Some(expected_model_id);
        if still_current {
            *self.lock_onnx() = Some(engine);
        } else {
            info!(
                "Model changed/unloaded during transcription; dropping stale engine (was '{}')",
                expected_model_id
            );
            // `engine` drops here, freeing its resources.
        }
    }

    /// Flush the active stream and return its final, post-filtered text.
    ///
    /// `Ok(None)` means no usable stream result exists and the caller should
    /// fall back to batch transcription. `Err` means the transcription was
    /// cancelled. There is no timeout here: waiting behind a model load is a
    /// queue position, not a failure (#1841), and the engine bounds the
    /// finalize itself by the audio it still has to decode.
    pub fn finalize_stream(&self) -> Result<Option<String>> {
        let Some(tx) = self.router.take() else {
            return Ok(None);
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.send(StreamCmd::Finalize(reply_tx)).is_err() {
            return Ok(None);
        }
        let finalized = match reply_rx.recv() {
            Ok(Ok(Some(finalized))) => finalized,
            Ok(Ok(None)) | Err(_) => return Ok(None),
            Ok(Err(e)) => return Err(e.into()),
        };

        let settings = get_settings(&self.app_handle);
        // Streaming models do not receive a decode prompt, so custom words
        // always go through the shared fuzzy post-correction path.
        let filtered = post_process_transcription_text(
            finalized.text,
            &settings,
            false,
            &finalized.output_language,
            &finalized.supported_languages,
        );

        self.maybe_unload_immediately("streaming transcription");
        Ok(Some(filtered))
    }

    /// Abandon any active stream without producing text (e.g. on cancel).
    pub fn cancel_stream(&self) {
        if let Some(tx) = self.router.take() {
            let _ = tx.send(StreamCmd::Cancel);
        }
        self.stream_active.store(false, Ordering::Release);
    }

    /// Stop the transcription in progress (a batch run or stream finalize),
    /// and any queued behind it, for an explicit user cancel. The worker doing
    /// it is stopped; the model stays loaded and the next use starts a fresh
    /// worker for it.
    ///
    /// File transcription jobs are not cancelled: a chunk this stops is
    /// retried by its job.
    pub fn cancel_transcription(&self) {
        self.transcription_cancels.fetch_add(1, Ordering::SeqCst);
        self.engine.cancel();
    }

    /// Emit a working-phase event to the streaming overlay (spinner + label).
    pub fn emit_stream_working(&self, kind: StreamWorkKind) {
        let _ = StreamPhaseEvent {
            phase: StreamPhase::Working,
            kind: Some(kind),
        }
        .emit(&self.app_handle);
    }

    pub fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        #[cfg(debug_assertions)]
        if std::env::var("HANDY_FORCE_TRANSCRIPTION_FAILURE").is_ok() {
            return Err(anyhow::anyhow!(
                "Simulated transcription failure (HANDY_FORCE_TRANSCRIPTION_FAILURE)"
            ));
        }

        // Update last activity timestamp
        self.touch_activity();

        let st = std::time::Instant::now();
        let audio_len = audio.len();

        debug!("Audio vector length: {}", audio_len);

        if audio.is_empty() {
            debug!("Empty audio vector");
            self.maybe_unload_immediately("empty audio");
            return Ok(String::new());
        }

        // Wait for a model load in progress. If the model is still not
        // loaded, it was unloaded after this dictation's start saw it loaded
        // (e.g. "Immediately" as a file job ended): load it now rather than
        // fail. Not under the run gate, which a load never waits for.
        if let Some(_loading) =
            claim_load_unless_loaded(&self.is_loading, &self.loading_condvar, || {
                self.is_model_loaded()
            })
        {
            self.reload_model_on_next_use
                .store(false, Ordering::Release);
            let settings = get_settings(&self.app_handle);
            info!(
                "Model is not loaded for transcription; loading '{}'",
                settings.selected_model
            );
            self.load_model(&settings.selected_model)
                .map_err(|e| anyhow::anyhow!("Model is not loaded for transcription: {}", e))?;
        }

        // Get current settings for configuration
        let settings = get_settings(&self.app_handle);

        // Validate selected language against the model's supported languages.
        // If the language isn't supported, fall back to "auto" to prevent errors.
        // Validate against the model that's actually loaded (which can differ
        // from settings.selected_model when a caller loaded a specific model —
        // e.g. the --transcribe-file path's --model), not the persisted
        // selection.
        let active_model = self
            .get_current_model()
            .unwrap_or_else(|| settings.selected_model.clone());
        // Resolve the persisted language *intent* into the language this model
        // will actually use. The coercion is capability-aware (a must-pick model
        // never receives "auto") and computed fresh here — it is never written
        // back to settings, so the intent survives switching models and back.
        let validated_language =
            effective_language_for_model(&settings, self.model_manager.as_ref(), &active_model);
        if validated_language != settings.selected_language {
            debug!(
                "Language intent '{}' resolved to '{}' for model '{}'",
                settings.selected_language, validated_language, active_model
            );
        }

        // Perform transcription with the appropriate engine, after any file
        // chunk in flight. A cancel while waiting for it stops this run too.
        let cancels = self.transcription_cancels.load(Ordering::SeqCst);
        let gate = self.lock_run_gate();
        if self.transcription_cancels.load(Ordering::SeqCst) != cancels {
            return Err(EngineError::Cancelled.into());
        }
        let run = self.run_engine(
            &gate,
            audio,
            &settings,
            &validated_language,
            &active_model,
            None,
        )?;
        drop(gate);

        let output_language = with_model_detected_language(
            resolve_output_language_evidence(
                &settings,
                run.applied_language_hint.as_deref(),
                &run.languages,
                run.output_was_translated,
            ),
            run.model_detected_language,
        );
        debug!("Output language evidence: {:?}", output_language);

        // Apply fuzzy word correction if custom words are configured — UNLESS the
        // words were already handed to the model as an initial prompt (whisper
        // family). We don't pass a prompt to non-whisper models (it requires the
        // whisper-kind run extension), so they still get fuzzy correction here,
        // same as the ONNX engines.
        let filtered_result = post_process_transcription_text(
            run.text,
            &settings,
            run.model_is_whisper,
            &output_language,
            &run.languages,
        );

        let et = std::time::Instant::now();
        let translation_note = if settings.translate_to_english {
            " (translated)"
        } else {
            ""
        };
        // Real-time factor. Input PCM is 16 kHz mono, so audio length in seconds
        // is samples / 16000. `speedup` is audio_secs / elapsed_secs — e.g. 4.00x
        // means transcribed 4x faster than real time
        let elapsed_secs = (et - st).as_secs_f64();
        let audio_secs = audio_len as f64 / 16_000.0;
        let speedup = real_time_factor(audio_secs, elapsed_secs);
        info!(
            "Transcription completed in {:.2}s for {:.2}s of audio ({:.2}x real-time){}",
            elapsed_secs, audio_secs, speedup, translation_note
        );

        let final_result = filtered_result;

        if final_result.is_empty() {
            info!("Transcription result is empty");
        } else {
            info!(
                "Transcription result: {}",
                crate::utils::redact_text(&final_result)
            );
        }

        self.maybe_unload_immediately("transcription");

        Ok(final_result)
    }

    /// One batch run on whichever engine holds the current model. `_gate`
    /// proves the caller holds the run gate. `file` is the job's token for a
    /// file chunk (`None` for dictation): its transcribe-cpp call is scoped
    /// to that token and asks for segment timestamps.
    fn run_engine(
        &self,
        _gate: &MutexGuard<'_, ()>,
        audio: Vec<f32>,
        settings: &AppSettings,
        validated_language: &str,
        active_model: &str,
        file: Option<&WorkToken>,
    ) -> Result<RunOutcome> {
        match self.engine.loaded() {
            Some(info) => self.transcribe_cpp(
                info,
                audio,
                settings,
                validated_language,
                active_model,
                file,
            ),
            None => self.transcribe_onnx(&audio, settings, validated_language, active_model),
        }
    }

    /// transcribe-cpp: the model runs in the engine's worker process, which
    /// recovers from crashes and hangs itself. The loaded model's live
    /// capabilities (cheap GGUF-metadata reads) are the source of truth, not
    /// the ModelManager copy. The whisper run extension is kind-tagged, so
    /// non-whisper archs (parakeet, voxtral, …) reject it with INVALID_ARG;
    /// attach it — and translate — only where supported.
    fn transcribe_cpp(
        &self,
        info: LoadedInfo,
        audio: Vec<f32>,
        settings: &AppSettings,
        validated_language: &str,
        active_model: &str,
        file: Option<&WorkToken>,
    ) -> Result<RunOutcome> {
        // Whether the model advertises Feature::InitialPrompt. Informational
        // (logged below); the whisper run extension and the fuzzy-correction
        // skip are gated on `model_is_whisper` instead, since non-whisper
        // archs can advertise the feature while rejecting the whisper-kind
        // extension.
        let model_takes_initial_prompt = info.supports_initial_prompt;
        let model_is_whisper = info.arch == "whisper";
        let model_supports_translate = info.capabilities.supports_translate;
        let max_timestamp_kind = info.capabilities.max_timestamp_kind;
        let languages = info.capabilities.languages;
        debug!(
            "transcribe-cpp model '{}' on '{}': initial_prompt={}, translate={}, languages={:?}",
            settings.selected_model,
            info.backend,
            model_takes_initial_prompt,
            model_supports_translate,
            languages
        );

        // Custom words become the initial prompt ONLY for models that accept
        // one (whisper family). Attaching the whisper run extension to a
        // non-whisper arch is rejected with INVALID_ARG, so skip it there and
        // let the fuzzy post-correction handle custom words instead.
        let family = if settings.custom_words.is_empty() || !model_is_whisper {
            None
        } else {
            Some(RunExtension::Whisper(WhisperRunOptions {
                initial_prompt: Some(settings.custom_words.join(", ")),
                ..Default::default()
            }))
        };

        let run_plan = transcribe_cpp_run_plan(
            settings.translate_to_english,
            validated_language,
            &languages,
            model_supports_translate,
        );
        let output_was_translated = run_plan.target_language.as_deref() == Some("en");
        let applied_language_hint = run_plan.language.clone();

        let mut run_options = RunOptions {
            task: run_plan.task,
            language: run_plan.language,
            target_language: run_plan.target_language,
            family,
            ..Default::default()
        };
        // Dictation keeps the default (Auto). File runs want segment times
        // for subtitles; see file_run_timestamps.
        if file.is_some() {
            run_options.timestamps = file_run_timestamps(max_timestamp_kind);
        }

        debug!(
            "transcribe-cpp run: task={:?}, language={:?}, initial_prompt={}, timestamps={:?}",
            run_options.task,
            run_options.language,
            run_options.family.is_some(),
            run_options.timestamps
        );

        let result = match file {
            Some(token) => self.engine.transcribe_scoped(audio, run_options, token),
            None => self.engine.transcribe(audio, run_options),
        };
        let transcript = match result {
            Ok(transcript) => transcript,
            Err(EngineError::Cancelled) => return Err(EngineError::Cancelled.into()),
            Err(e) => {
                self.forget_model_if_engine_dropped(active_model, &e.to_string());
                return Err(anyhow::anyhow!(
                    "transcribe-cpp transcription failed: {}",
                    e
                ));
            }
        };
        // Speaker-attributed models may return untimed rows even without
        // timestamps; only rows of a timed run carry usable times.
        let segments = if transcript.timestamp_kind == TimestampKind::None {
            Vec::new()
        } else {
            transcript
                .segments
                .into_iter()
                .map(|segment| RawTimed {
                    start_ms: segment.t0_ms,
                    end_ms: segment.t1_ms,
                    text: segment.text,
                })
                .collect()
        };
        Ok(RunOutcome {
            text: transcript.text,
            segments,
            languages,
            applied_language_hint,
            output_was_translated,
            // Whisper's audio-based LID (auto mode only; `None` when a
            // language hint was passed).
            model_detected_language: transcript.language,
            model_is_whisper,
        })
    }

    /// In-process ONNX engines.
    fn transcribe_onnx(
        &self,
        audio: &[f32],
        settings: &AppSettings,
        validated_language: &str,
        active_model: &str,
    ) -> Result<RunOutcome> {
        let languages = self
            .model_manager
            .get_model_info(active_model)
            .map(|info| info.supported_languages)
            .unwrap_or_default();
        let mut output_was_translated = false;
        let mut applied_language_hint: Option<String> = None;

        // Take the engine out so we own it during transcription. If the
        // engine panics, we simply don't put it back (effectively unloading
        // it) instead of poisoning the mutex. No lock is held during the
        // engine call, only the run gate (by the caller), so no one else
        // takes it meanwhile. Marked in use first, so the model never reads
        // as unloaded while it is out; the guard clears the mark on every
        // exit, after the engine went back.
        self.onnx_in_use.store(true, Ordering::Release);
        let _in_use = OnnxInUse(Arc::clone(&self.onnx_in_use));
        let mut engine = match self.lock_onnx().take() {
            Some(e) => e,
            None => {
                return Err(anyhow::anyhow!(
                    "Model failed to load after auto-load attempt. Please check your model settings."
                ));
            }
        };

        // Parakeet is asked for segment-level rows (already shifted back by
        // its leading pad). Other engines' rows are token-level (SenseVoice)
        // or absent, so their runs carry none.
        let mut parakeet_segments = Vec::new();

        // We use catch_unwind to prevent engine panics from poisoning the
        // mutex, which would make the app hang indefinitely on subsequent
        // operations.
        let transcribe_result = catch_unwind(AssertUnwindSafe(|| -> Result<String> {
            match &mut engine {
                OnnxEngine::Parakeet(parakeet_engine) => {
                    let params = ParakeetParams {
                        timestamp_granularity: Some(TimestampGranularity::Segment),
                        ..Default::default()
                    };
                    parakeet_engine
                        .transcribe_with(audio, &params)
                        .map(|r| {
                            parakeet_segments = r.segments.unwrap_or_default();
                            r.text
                        })
                        .map_err(|e| anyhow::anyhow!("Parakeet transcription failed: {}", e))
                }
                OnnxEngine::Moonshine(moonshine_engine) => moonshine_engine
                    .transcribe(audio, &TranscribeOptions::default())
                    .map(|r| r.text)
                    .map_err(|e| anyhow::anyhow!("Moonshine transcription failed: {}", e)),
                OnnxEngine::MoonshineStreaming(streaming_engine) => streaming_engine
                    .transcribe(audio, &TranscribeOptions::default())
                    .map(|r| r.text)
                    .map_err(|e| {
                        anyhow::anyhow!("Moonshine streaming transcription failed: {}", e)
                    }),
                OnnxEngine::SenseVoice(sense_voice_engine) => {
                    let language = match validated_language {
                        "zh" => Some("zh".to_string()),
                        "en" => Some("en".to_string()),
                        "ja" => Some("ja".to_string()),
                        "ko" => Some("ko".to_string()),
                        "yue" => Some("yue".to_string()),
                        _ => None,
                    };
                    applied_language_hint = language.clone();
                    let params = SenseVoiceParams {
                        language,
                        use_itn: Some(true),
                    };
                    sense_voice_engine
                        .transcribe_with(audio, &params)
                        .map(|r| r.text)
                        .map_err(|e| anyhow::anyhow!("SenseVoice transcription failed: {}", e))
                }
                OnnxEngine::GigaAM(gigaam_engine) => gigaam_engine
                    .transcribe(audio, &TranscribeOptions::default())
                    .map(|r| r.text)
                    .map_err(|e| anyhow::anyhow!("GigaAM transcription failed: {}", e)),
                OnnxEngine::Canary(canary_engine) => {
                    output_was_translated = settings.translate_to_english;
                    let lang = if validated_language == "auto" {
                        None
                    } else {
                        Some(validated_language.to_string())
                    };
                    applied_language_hint = lang.clone();
                    let options = TranscribeOptions {
                        language: lang,
                        translate: settings.translate_to_english,
                        ..Default::default()
                    };
                    canary_engine
                        .transcribe(audio, &options)
                        .map(|r| r.text)
                        .map_err(|e| anyhow::anyhow!("Canary transcription failed: {}", e))
                }
                OnnxEngine::Cohere(cohere_engine) => {
                    let lang = if validated_language == "auto" {
                        None
                    } else {
                        Some(validated_language.to_string())
                    };
                    applied_language_hint = lang.clone();
                    let options = TranscribeOptions {
                        language: lang,
                        ..Default::default()
                    };
                    cohere_engine
                        .transcribe(audio, &options)
                        .map(|r| r.text)
                        .map_err(|e| anyhow::anyhow!("Cohere transcription failed: {}", e))
                }
            }
        }));

        let text = match transcribe_result {
            Ok(inner_result) => {
                // Success or normal error: return the engine unless a model
                // switch/unload invalidated it while it was in use.
                self.return_engine(engine, active_model);
                inner_result?
            }
            Err(panic_payload) => {
                // Engine panicked — do NOT put it back (it's in an unknown state).
                // The engine is dropped here, effectively unloading it.
                let panic_msg = panic_payload_message(panic_payload.as_ref());
                error!(
                    "Transcription engine panicked: {}. Model has been unloaded.",
                    panic_msg
                );

                // Clear the model ID so it will be reloaded on next attempt
                {
                    let mut current_model = self
                        .current_model_id
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    *current_model = None;
                }

                let _ = self.app_handle.emit(
                    "model-state-changed",
                    ModelStateEvent {
                        event_type: "unloaded".to_string(),
                        model_id: None,
                        model_name: None,
                        error: Some(format!("Engine panicked: {}", panic_msg)),
                    },
                );

                return Err(anyhow::anyhow!(
                    "Transcription engine panicked: {}. The model has been unloaded and will reload on next attempt.",
                    panic_msg
                ));
            }
        };
        let segments = parakeet_segments
            .into_iter()
            .map(|segment| RawTimed {
                start_ms: (f64::from(segment.start) * 1000.0).round() as i64,
                end_ms: (f64::from(segment.end) * 1000.0).round() as i64,
                text: segment.text,
            })
            .collect();
        Ok(RunOutcome {
            text,
            segments,
            languages,
            applied_language_hint,
            output_was_translated,
            model_detected_language: None,
            model_is_whisper: false,
        })
    }
}

/// What a batch transcription produced, plus what post-processing needs to
/// know about how it was produced.
struct RunOutcome {
    text: String,
    /// Timed segment rows of the run, if the engine produced usable ones.
    segments: Vec<RawTimed>,
    /// The model's supported languages.
    languages: Vec<String>,
    applied_language_hint: Option<String>,
    output_was_translated: bool,
    /// The language the model itself detected, if it reported one.
    model_detected_language: Option<String>,
    /// Whether the loaded model is actually whisper-family (arch string).
    /// Non-whisper archs (e.g. Voxtral Small) can advertise
    /// Feature::InitialPrompt yet reject the whisper-kind run extension with
    /// INVALID_ARG, so the whisper extension must be gated on the arch, not
    /// on the feature (see #1601).
    model_is_whisper: bool,
}

/// One timed row from an engine, in ms relative to the run's audio.
#[derive(Clone, Debug, PartialEq)]
struct RawTimed {
    start_ms: i64,
    end_ms: i64,
    text: String,
}

// ---------------------------------------------------------------------------
// File transcription (see `crate::file_transcription`). A job holds a
// `FileJobLease` for its whole run and sends its audio one chunk per engine
// call, between dictations: it never dispatches while one is active, and a
// cancel of the job never touches a dictation's work.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkLimits {
    /// Default longest chunk for this model (28 000; 24 000 GigaAM; ≤ hard cap).
    pub max_chunk_ms: u32,
    /// Cut search window before max_chunk_ms (min(10 000, max_chunk_ms / 2)).
    pub search_window_ms: u32,
    /// Longest audio the model accepts or handles without truncation, if limited.
    pub hard_max_ms: Option<u32>,
}

pub struct ChunkTranscript {
    /// Post-processed text of the whole chunk (same processing as a dictation). "" if nothing was said.
    pub text: String,
    /// Post-processed timed segments, absolute ms, sorted, inside
    /// [offset_ms, offset_ms + real_len_ms]. Empty iff `text` is empty.
    pub segments: Vec<TranscriptSegment>,
    pub grain: TimingGrain,
    /// ISO code of the resolved output language, if known.
    pub language: Option<String>,
}

#[derive(Debug)]
pub enum ChunkError {
    /// Dictation is active; nothing was sent. Wait and call again.
    Yield,
    /// The job's own WorkToken was cancelled.
    Cancelled,
    /// A different model became current during the job.
    ModelChanged {
        job_model: String,
        current_model: String,
    },
    /// Load or engine failure (foreign cancels and reloads were already retried).
    Failed(anyhow::Error),
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Yield => f.write_str("waiting for a dictation to finish"),
            ChunkError::Cancelled => f.write_str("file transcription was cancelled"),
            ChunkError::ModelChanged {
                job_model,
                current_model,
            } => write!(
                f,
                "the transcription model changed from '{}' to '{}'",
                job_model, current_model
            ),
            ChunkError::Failed(error) => write!(f, "{:#}", error),
        }
    }
}

impl std::error::Error for ChunkError {}

/// Holds the model for a file job: while any lease exists the model is not
/// unloaded by "Immediately", the idle watcher, or dictation's FinishGuard.
pub struct FileJobLease {
    tm: TranscriptionManager,
    model_id: String,
    device_index: Option<usize>,
    settings: AppSettings, // snapshot at job start
    token: WorkToken,
    /// The first confident output language of the job, used for chunks
    /// whose own run gives no evidence.
    evidence: Mutex<Option<OutputLanguageEvidence>>,
    limits: ChunkLimits,
}

impl FileJobLease {
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn token(&self) -> &WorkToken {
        &self.token
    }

    pub fn limits(&self) -> ChunkLimits {
        self.limits
    }

    /// Blocking. Never holds the run gate while waiting or loading.
    ///
    /// `samples` is 16 kHz mono (possibly zero-padded); `offset_ms` is where
    /// the chunk starts in the file and `real_len_ms` its length before
    /// padding. A chunk stopped by a global cancel (Escape during a
    /// dictation) is retried, and the job's model is reloaded if it was
    /// unloaded or its engine was lost.
    pub fn transcribe_chunk(
        &self,
        samples: &[f32],
        offset_ms: u64,
        real_len_ms: u64,
    ) -> Result<ChunkTranscript, ChunkError> {
        let tm = &self.tm;
        let mut foreign_cancels = 0;
        let mut reloads = 0;
        loop {
            if self.token.is_cancelled() {
                return Err(ChunkError::Cancelled);
            }
            if tm.dictation_active() {
                return Err(ChunkError::Yield);
            }
            tm.ensure_file_model(&self.model_id, self.device_index, &self.token, false)?;

            let gate = tm.lock_run_gate();
            // Checked again under the gate: a dictation that began since must
            // not wait behind this chunk. Its flags are set before it needs
            // the gate, so after this check it can only queue behind us.
            if tm.dictation_active() {
                return Err(ChunkError::Yield);
            }
            if self.token.is_cancelled() {
                return Err(ChunkError::Cancelled);
            }
            match tm.get_current_model() {
                Some(current) if current != self.model_id => {
                    return Err(ChunkError::ModelChanged {
                        job_model: self.model_id.clone(),
                        current_model: current,
                    });
                }
                Some(_) if tm.is_model_loaded() => {}
                // Unloaded (or being replaced) since ensure_file_model.
                _ => {
                    drop(gate);
                    reloads += 1;
                    if reloads > FILE_CHUNK_RELOADS {
                        return Err(ChunkError::Failed(anyhow::anyhow!(
                            "the model '{}' did not stay loaded",
                            self.model_id
                        )));
                    }
                    continue;
                }
            }
            tm.touch_activity();
            let language =
                effective_language_for_model(&self.settings, &tm.model_manager, &self.model_id);
            let result = tm.run_engine(
                &gate,
                samples.to_vec(),
                &self.settings,
                &language,
                &self.model_id,
                Some(&self.token),
            );
            drop(gate);

            match result {
                Ok(run) => return Ok(self.shape(run, offset_ms, real_len_ms)),
                Err(error) if is_engine_cancel(&error) => {
                    if self.token.is_cancelled() {
                        return Err(ChunkError::Cancelled);
                    }
                    // A global cancel meant for a dictation stopped this chunk.
                    foreign_cancels += 1;
                    if foreign_cancels > FILE_CHUNK_FOREIGN_CANCEL_RETRIES {
                        return Err(ChunkError::Failed(error));
                    }
                    debug!(
                        "File chunk at {}ms was cancelled by a dictation cancel; retrying",
                        offset_ms
                    );
                }
                // The engine dropped the model (an ONNX panic, or transcribe-cpp
                // could not restart its worker): load it again and retry.
                Err(error) if !tm.is_model_loaded() && reloads < FILE_CHUNK_RELOADS => {
                    reloads += 1;
                    debug!(
                        "File chunk at {}ms failed with the model unloaded ({:#}); reloading",
                        offset_ms, error
                    );
                }
                Err(error) => return Err(ChunkError::Failed(error)),
            }
        }
    }

    /// Post-process a finished run like a dictation, and place its timed
    /// segments in the file. Logs at debug only: the CLI shows the log.
    fn shape(&self, run: RunOutcome, offset_ms: u64, real_len_ms: u64) -> ChunkTranscript {
        let settings = &self.settings;
        let run_evidence = with_model_detected_language(
            resolve_output_language_evidence(
                settings,
                run.applied_language_hint.as_deref(),
                &run.languages,
                run.output_was_translated,
            ),
            run.model_detected_language,
        );
        let evidence = {
            let mut stored = self.evidence.lock().unwrap_or_else(|e| e.into_inner());
            resolve_chunk_evidence(run_evidence, &mut stored, || {
                if wants_text_language_detection(settings) {
                    detect_language_fail_open(&run.text, &run.languages)
                } else {
                    None
                }
            })
        };

        // The language is resolved once for the whole chunk; segments are
        // never detected one by one.
        let process = |raw: String| {
            post_process_resolved_text(raw, settings, run.model_is_whisper, &evidence)
        };
        let text = process(run.text.clone()).trim().to_string();
        let (segments, grain) = place_segments(
            &text,
            &run.text,
            run.segments,
            offset_ms,
            real_len_ms,
            process,
        );
        debug!(
            "File chunk at {}ms ({}ms): {} chars, {} segment(s), {:?} timing, language evidence {:?}",
            offset_ms,
            real_len_ms,
            text.chars().count(),
            segments.len(),
            grain,
            evidence
        );
        ChunkTranscript {
            text,
            segments,
            grain,
            language: evidence.language().map(str::to_string),
        }
    }
}

impl Drop for FileJobLease {
    fn drop(&mut self) {
        let previous = self.tm.active_file_jobs.fetch_sub(1, Ordering::SeqCst);
        // An active dictation unloads the model itself when it finishes
        // (FinishGuard, see `release_dictation_hold`).
        if previous == 1 && !self.tm.dictation_active() {
            self.tm.maybe_unload_immediately("file transcription");
        }
    }
}

/// Marks a dictation as in progress from TranscribeAction::stop until its pipeline finishes.
pub struct DictationHold(Arc<AtomicUsize>);

impl Drop for DictationHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl TranscriptionManager {
    /// Blocking. Increments active_file_jobs first. Ensures `model_id` is the
    /// loaded model (loading it under try_start_loading()/LoadingGuard, with
    /// load_model_with_device(model_id, device_index), replacing a different
    /// loaded model), snapshots settings, computes limits.
    ///
    /// A different model is only replaced once no dictation is using it. A
    /// failed load returns its own error (e.g. "Model not found: …" or
    /// "Model not downloaded"); a cancel of `token` while waiting returns
    /// [`ChunkError::Cancelled`] (downcast the error to tell).
    pub fn begin_file_job(
        &self,
        model_id: &str,
        device_index: Option<usize>,
        token: WorkToken,
    ) -> anyhow::Result<FileJobLease> {
        self.active_file_jobs.fetch_add(1, Ordering::SeqCst);
        // From here on the lease's Drop gives the count back, however this
        // ends.
        let mut lease = FileJobLease {
            tm: self.clone(),
            model_id: model_id.to_string(),
            device_index,
            settings: get_settings(&self.app_handle),
            token,
            evidence: Mutex::new(None),
            limits: compute_chunk_limits(false, None),
        };
        self.ensure_file_model(model_id, device_index, &lease.token, true)
            .map_err(|error| match error {
                ChunkError::Failed(error) => error,
                other => anyhow::Error::new(other),
            })?;
        lease.limits = self.chunk_limits_for(model_id);
        self.touch_activity();
        debug!(
            "File transcription started with model '{}' ({:?})",
            model_id, lease.limits
        );
        Ok(lease)
    }

    /// Recording, live stream (router open / stream active / stream worker),
    /// or a DictationHold outstanding.
    pub fn dictation_active(&self) -> bool {
        self.router.is_open()
            || self.stream_active.load(Ordering::Acquire)
            || self.active_stream_worker.load(Ordering::Acquire) != 0
            || self.dictation_holds.load(Ordering::SeqCst) > 0
            || self
                .app_handle
                .try_state::<Arc<AudioRecordingManager>>()
                .is_some_and(|audio| audio.is_recording())
    }

    pub fn hold_for_dictation(&self) -> DictationHold {
        self.dictation_holds.fetch_add(1, Ordering::SeqCst);
        DictationHold(Arc::clone(&self.dictation_holds))
    }

    /// Release a dictation's hold once its pipeline has finished (after its
    /// own [`maybe_unload_immediately`](Self::maybe_unload_immediately)). A
    /// file job that ended while the hold was out left the unload to the
    /// dictation; if the dictation's own unload was deferred for that job, it
    /// is done here. Without file jobs this does nothing but release.
    pub fn release_dictation_hold(&self, hold: DictationHold) {
        drop(hold);
        // Pairs with the lease's decrement-then-check: one of the two sees
        // the other's release, so the unload is never skipped by both.
        fence(Ordering::SeqCst);
        if self.active_file_jobs.load(Ordering::SeqCst) == 0
            && self.unload_deferred.load(Ordering::SeqCst)
            && !self.dictation_active()
        {
            self.maybe_unload_immediately("transcription session");
        }
    }

    /// token.cancel() plus kill only that token's in-flight transcribe-cpp call.
    pub fn cancel_file_work(&self, token: &WorkToken) {
        self.engine.cancel_work(token);
    }

    pub fn active_file_jobs(&self) -> usize {
        self.active_file_jobs.load(Ordering::SeqCst)
    }

    /// Make `model_id` the loaded model for a file job, without the run
    /// gate. Waits for a load in progress, checking `token` every 200 ms.
    /// With no model loaded it loads `model_id`. A different loaded model is
    /// replaced when `replace` (job start), or is [`ChunkError::ModelChanged`]
    /// otherwise (the user switched models mid-job).
    fn ensure_file_model(
        &self,
        model_id: &str,
        device_index: Option<usize>,
        token: &WorkToken,
        replace: bool,
    ) -> Result<(), ChunkError> {
        // Whether `model_id` is loaded as is. A pending accelerator change
        // asks for a reload on next use, which a job start applies.
        let ready = || {
            let reload = replace && self.reload_model_on_next_use.load(Ordering::Acquire);
            !reload
                && self.get_current_model().as_deref() == Some(model_id)
                && self.is_model_loaded()
        };
        loop {
            self.wait_for_model_load(token)?;
            // The common case: nothing to load, and the loading flag is not
            // touched (taking it would make a dictation's load or a model
            // switch think one is already running).
            if ready() {
                return Ok(());
            }
            let current = self.get_current_model();
            match current {
                Some(current) if current != model_id && !replace => {
                    return Err(ChunkError::ModelChanged {
                        job_model: model_id.to_string(),
                        current_model: current,
                    });
                }
                // Replacing the model would pull it out from under the
                // dictation using it; wait for that dictation to finish.
                Some(current) if current != model_id && self.dictation_active() => {
                    thread::sleep(FILE_JOB_DICTATION_POLL);
                    continue;
                }
                _ => {}
            }
            // Another load started since the wait: wait for it, then decide
            // again.
            let Some(_loading) = self.try_start_loading() else {
                continue;
            };
            // Decide again under the loading flag: a load that finished in
            // between may have made this one unnecessary, or loaded a model
            // the user switched to, which a running job must not replace.
            if ready() {
                return Ok(());
            }
            if let Some(current) = self.get_current_model() {
                if current != model_id && !replace {
                    return Err(ChunkError::ModelChanged {
                        job_model: model_id.to_string(),
                        current_model: current,
                    });
                }
            }
            if token.is_cancelled() {
                return Err(ChunkError::Cancelled);
            }
            if replace {
                self.reload_model_on_next_use
                    .store(false, Ordering::Release);
            }
            debug!("Loading model '{}' for file transcription", model_id);
            return self
                .load_model_with_device(model_id, device_index)
                .map_err(ChunkError::Failed);
        }
    }

    /// Wait until no model load is in progress, or `token` is cancelled.
    fn wait_for_model_load(&self, token: &WorkToken) -> Result<(), ChunkError> {
        let mut is_loading = self.is_loading.lock().unwrap_or_else(|e| e.into_inner());
        while *is_loading {
            if token.is_cancelled() {
                return Err(ChunkError::Cancelled);
            }
            is_loading = match self
                .loading_condvar
                .wait_timeout(is_loading, FILE_JOB_LOAD_POLL)
            {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        if token.is_cancelled() {
            return Err(ChunkError::Cancelled);
        }
        Ok(())
    }

    /// Chunk limits for `model_id`, which is the loaded model.
    fn chunk_limits_for(&self, model_id: &str) -> ChunkLimits {
        let engine_type = self
            .model_manager
            .get_model_info(model_id)
            .map(|info| info.engine_type);
        let (is_gigaam, cap) = match engine_type {
            Some(EngineType::TranscribeCpp) | None => match self.engine.loaded() {
                Some(info) => transcribe_cpp_chunk_cap(&info.arch, info.capabilities.max_audio_ms),
                None => (false, None),
            },
            // The ONNX engine may be checked out, so go by the model's type.
            Some(engine_type) => onnx_chunk_cap(&engine_type),
        };
        compute_chunk_limits(is_gigaam, cap)
    }
}

/// The timestamps a file run asks transcribe-cpp for: segment level when the
/// model has any timestamps (a request coarser than the model's finest is
/// served at exactly that level), none when it has none. `Auto` stays Auto.
fn file_run_timestamps(max_timestamp_kind: TimestampKind) -> TimestampKind {
    match max_timestamp_kind {
        TimestampKind::None => TimestampKind::None,
        TimestampKind::Auto => TimestampKind::Auto,
        TimestampKind::Segment | TimestampKind::Word | TimestampKind::Token => {
            TimestampKind::Segment
        }
    }
}

/// Whether `error` is an engine run stopped by a cancel.
fn is_engine_cancel(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<EngineError>(),
        Some(EngineError::Cancelled)
    )
}

/// `(is_gigaam, cap)` for a transcribe-cpp model: one second under its own
/// longest accepted audio, and GigaAM's 24 s.
fn transcribe_cpp_chunk_cap(arch: &str, max_audio_ms: i64) -> (bool, Option<u32>) {
    let is_gigaam = arch.to_ascii_lowercase().contains("gigaam");
    let model_cap =
        (max_audio_ms > 0).then(|| u32::try_from((max_audio_ms - 1000).max(0)).unwrap_or(u32::MAX));
    let cap = match (model_cap, is_gigaam) {
        (Some(cap), true) => Some(cap.min(24_000)),
        (None, true) => Some(24_000),
        (cap, false) => cap,
    };
    (is_gigaam, cap)
}

/// `(is_gigaam, cap)` for an in-process ONNX engine, from what each one
/// accepts (or handles without silently truncating its output).
fn onnx_chunk_cap(engine_type: &EngineType) -> (bool, Option<u32>) {
    match engine_type {
        // Errors above 64 s.
        EngineType::Moonshine => (false, Some(63_000)),
        // Their decoders stop at a fixed token count.
        EngineType::MoonshineStreaming | EngineType::Canary | EngineType::Cohere => {
            (false, Some(30_000))
        }
        EngineType::SenseVoice => (false, Some(30_000)),
        EngineType::GigaAM => (true, Some(24_000)),
        // Attention cost grows with the square of the length.
        EngineType::Parakeet => (false, Some(120_000)),
        EngineType::TranscribeCpp => (false, None),
    }
}

/// The default chunking for a model: 28 s (24 s for GigaAM), never above the
/// model's cap (but at least 5 s), cutting within the last 10 s (at most half
/// the chunk).
fn compute_chunk_limits(is_gigaam: bool, engine_cap_ms: Option<u32>) -> ChunkLimits {
    let base = if is_gigaam { 24_000 } else { 28_000 };
    let max_chunk_ms = match engine_cap_ms {
        Some(cap) => base.min(cap.max(5_000)),
        None => base,
    };
    ChunkLimits {
        max_chunk_ms,
        search_window_ms: (max_chunk_ms / 2).min(10_000),
        hard_max_ms: engine_cap_ms,
    }
}

/// The job-level language evidence for one chunk: the run's own evidence,
/// else the job's first confident evidence, else (if `detect` finds one) the
/// language detected from the chunk's text. The first confident evidence is
/// stored for later chunks.
fn resolve_chunk_evidence(
    run_evidence: OutputLanguageEvidence,
    stored: &mut Option<OutputLanguageEvidence>,
    detect: impl FnOnce() -> Option<String>,
) -> OutputLanguageEvidence {
    let mut evidence = run_evidence;
    if evidence == OutputLanguageEvidence::Unknown {
        if let Some(known) = stored.as_ref() {
            evidence = known.clone();
        }
    }
    if evidence == OutputLanguageEvidence::Unknown {
        if let Some(language) = detect() {
            evidence = OutputLanguageEvidence::TextDetected(language);
        }
    }
    if evidence != OutputLanguageEvidence::Unknown && stored.is_none() {
        *stored = Some(evidence.clone());
    }
    evidence
}

/// [`detect_output_language`] that treats a panic as "not detected": it is
/// optional evidence and must never fail a chunk.
fn detect_language_fail_open(text: &str, supported_languages: &[String]) -> Option<String> {
    match catch_unwind(AssertUnwindSafe(|| {
        detect_output_language(text, supported_languages)
    })) {
        Ok(language) => language,
        Err(payload) => {
            error!(
                "Text language detection panicked: {}",
                panic_payload_message(payload.as_ref())
            );
            None
        }
    }
}

/// Whether the engine's segment rows hold at least half of the run's text
/// (ignoring whitespace). Rows that miss more than that are incomplete and
/// would drop text from subtitles, so the chunk is timed as a whole instead.
fn segments_cover_text(segments: &[RawTimed], text: &str) -> bool {
    let visible = |s: &str| s.chars().filter(|c| !c.is_whitespace()).count();
    let text_chars = visible(text);
    let segment_chars: usize = segments.iter().map(|segment| visible(&segment.text)).sum();
    text_chars > 0 && segment_chars * 2 >= text_chars
}

/// Place a chunk's segment rows in the file. `text` is the chunk's
/// post-processed text and `raw_text` the engine's text before
/// post-processing; `process` post-processes one segment's text. Rows are
/// kept when non-empty and timed (`end ≥ start ≥ 0`; inverted rows are
/// dropped), offset by `offset_ms` and clamped to the chunk's real length; a
/// row left without duration gets up to [`MIN_SEGMENT_MS`]. Without usable
/// rows the whole chunk is one segment ([`TimingGrain::Chunk`]); an empty
/// `text` has none.
fn place_segments(
    text: &str,
    raw_text: &str,
    raw: Vec<RawTimed>,
    offset_ms: u64,
    real_len_ms: u64,
    mut process: impl FnMut(String) -> String,
) -> (Vec<TranscriptSegment>, TimingGrain) {
    // An empty chunk has nothing to time; it never lowers a job's grain.
    if text.is_empty() {
        return (Vec::new(), TimingGrain::Segment);
    }
    let chunk_end = offset_ms + real_len_ms;
    let timed: Vec<RawTimed> = raw
        .into_iter()
        .filter(|row| row.start_ms >= 0 && row.end_ms >= row.start_ms)
        .filter(|row| !row.text.trim().is_empty())
        .collect();
    let mut segments = Vec::new();
    if segments_cover_text(&timed, raw_text) {
        for row in timed {
            let text = process(row.text).trim().to_string();
            if text.is_empty() {
                continue;
            }
            let start_ms = (offset_ms + row.start_ms as u64).min(chunk_end);
            let mut end_ms = (offset_ms + row.end_ms as u64).min(chunk_end);
            if end_ms <= start_ms {
                end_ms = (start_ms + MIN_SEGMENT_MS).min(chunk_end);
                if end_ms <= start_ms {
                    continue;
                }
            }
            segments.push(TranscriptSegment {
                start_ms,
                end_ms,
                text,
            });
        }
        segments.sort_by_key(|segment| segment.start_ms);
    }
    if segments.is_empty() {
        let whole = TranscriptSegment {
            start_ms: offset_ms,
            end_ms: chunk_end.max(offset_ms + 1),
            text: text.to_string(),
        };
        return (vec![whole], TimingGrain::Chunk);
    }
    (segments, TimingGrain::Segment)
}

fn emit_stream_text(app_handle: &AppHandle, committed: &str, tentative: &str) {
    let _ = StreamTextEvent {
        committed: committed.to_string(),
        tentative: tentative.to_string(),
    }
    .emit(app_handle);
}

fn lock_perf(perf: &Mutex<StreamPerf>) -> MutexGuard<'_, StreamPerf> {
    perf.lock().unwrap_or_else(|e| e.into_inner())
}

struct StreamPerf {
    feed_count: u64,
    emit_count: u64,
    streamed_samples: u64,
    stream_compute_elapsed: Duration,
    last_log: Instant,
    latest_revision: i32,
    latest_input_received_ms: i64,
    latest_audio_committed_ms: i64,
    latest_buffered_ms: i64,
}

impl StreamPerf {
    fn new() -> Self {
        Self {
            feed_count: 0,
            emit_count: 0,
            streamed_samples: 0,
            stream_compute_elapsed: Duration::ZERO,
            last_log: Instant::now(),
            latest_revision: 0,
            latest_input_received_ms: 0,
            latest_audio_committed_ms: 0,
            latest_buffered_ms: 0,
        }
    }

    fn record_feed(&mut self, samples: usize) {
        self.feed_count += 1;
        self.streamed_samples += samples as u64;
    }

    fn record_compute(&mut self, elapsed: Duration) {
        self.stream_compute_elapsed += elapsed;
    }

    fn record_update(&mut self, update: &transcribe_cpp::StreamUpdate) {
        self.latest_revision = update.revision;
        self.latest_input_received_ms = update.input_received_ms;
        self.latest_audio_committed_ms = update.audio_committed_ms;
        self.latest_buffered_ms = update.buffered_ms;
    }

    fn record_emit(&mut self) {
        self.emit_count += 1;
    }

    fn maybe_log(&mut self) {
        if self.last_log.elapsed() < STREAM_PERF_LOG_INTERVAL {
            return;
        }

        let audio_secs = self.audio_secs();
        let compute_secs = self.compute_secs();
        debug!(
            "Live preview perf: {:.2}s streamed audio, {:.2}s model compute ({:.2}x real-time), \
             input_received={:.2}s, committed_audio={:.2}s, buffered={}ms, revision={}, \
             {} frames fed, {} updates emitted",
            audio_secs,
            compute_secs,
            real_time_factor(audio_secs, compute_secs),
            self.latest_input_received_ms as f64 / 1000.0,
            self.latest_audio_committed_ms as f64 / 1000.0,
            self.latest_buffered_ms,
            self.latest_revision,
            self.feed_count,
            self.emit_count,
        );
        self.last_log = Instant::now();
    }

    fn log_finalized(&self, chars: usize) {
        let audio_secs = self.audio_secs();
        let compute_secs = self.compute_secs();
        info!(
            "Live preview finalized in {:.2}s model compute for {:.2}s streamed audio ({:.2}x real-time): \
             input_received={:.2}s, committed_audio={:.2}s, buffered={}ms, revision={}, \
             {} frames fed, {} updates emitted, {} chars",
            compute_secs,
            audio_secs,
            real_time_factor(audio_secs, compute_secs),
            self.latest_input_received_ms as f64 / 1000.0,
            self.latest_audio_committed_ms as f64 / 1000.0,
            self.latest_buffered_ms,
            self.latest_revision,
            self.feed_count,
            self.emit_count,
            chars
        );
    }

    fn audio_secs(&self) -> f64 {
        self.streamed_samples as f64 / 16_000.0
    }

    fn compute_secs(&self) -> f64 {
        self.stream_compute_elapsed.as_secs_f64()
    }
}

fn real_time_factor(audio_secs: f64, compute_secs: f64) -> f64 {
    if compute_secs > 0.0 {
        audio_secs / compute_secs
    } else {
        0.0
    }
}

/// Resolve the persisted language intent into the language a specific model can
/// use without writing the coerced value back to settings.
fn effective_language_for_model(
    settings: &AppSettings,
    model_manager: &ModelManager,
    model_id: &str,
) -> String {
    match model_manager.get_model_info(model_id) {
        Some(info) => crate::managers::model::effective_language(
            &settings.selected_language,
            &info.supported_languages,
            info.supports_language_detection,
        ),
        None => settings.selected_language.clone(),
    }
}

/// Resolve how confidently Handy knows the language of the text produced by a
/// transcription run. The UI language is deliberately not part of this
/// decision.
fn resolve_output_language_evidence(
    settings: &AppSettings,
    applied_language_hint: Option<&str>,
    supported_languages: &[String],
    translated_to_english: bool,
) -> OutputLanguageEvidence {
    if translated_to_english {
        return OutputLanguageEvidence::TranslatedToEnglish;
    }

    // Stored language intent is only evidence when this specific engine run
    // actually received the hint. Some multilingual engines (notably Parakeet
    // V3) always auto-detect and ignore Handy's selection; transcribe-cpp also
    // drops a requested hint when the loaded model does not advertise it.
    if let Some(language) = applied_language_hint.filter(|lang| !lang.is_empty() && *lang != "auto")
    {
        if settings.selected_language != "auto"
            && crate::managers::model::canonical_language_code(&settings.selected_language)
                == crate::managers::model::canonical_language_code(language)
        {
            return OutputLanguageEvidence::UserSelected(language.to_string());
        }

        // The engine may have required a concrete fallback even though the
        // user's persisted language was auto or unsupported.
        return OutputLanguageEvidence::ModelConstrained(language.to_string());
    }

    // A single-language model has a known output language without needing a
    // selectable language hint.
    if let [language] = supported_languages {
        return OutputLanguageEvidence::ModelConstrained(language.clone());
    }

    OutputLanguageEvidence::Unknown
}

/// Upgrade [`OutputLanguageEvidence::Unknown`] with the language the model
/// itself detected during the run (audio-based LID, e.g. Whisper in auto
/// mode). Stronger evidence resolved before the run is never overridden.
fn with_model_detected_language(
    evidence: OutputLanguageEvidence,
    detected: Option<String>,
) -> OutputLanguageEvidence {
    match (evidence, detected) {
        (OutputLanguageEvidence::Unknown, Some(language))
            if !language.is_empty() && language != "auto" =>
        {
            OutputLanguageEvidence::ModelDetected(language)
        }
        (evidence, _) => evidence,
    }
}

struct TranscribeCppRunPlan {
    task: Task,
    language: Option<String>,
    target_language: Option<String>,
}

/// Build the transcribe-cpp language/task options shared by batch and live
/// streaming paths.
fn transcribe_cpp_run_plan(
    translate_to_english: bool,
    effective_language: &str,
    model_languages: &[String],
    model_supports_translate: bool,
) -> TranscribeCppRunPlan {
    let requested_language = match effective_language {
        "auto" => None,
        other => Some(other.to_string()),
    };
    // Only pass a language the loaded model actually advertises (per
    // capabilities().languages); otherwise auto-detect rather than failing with
    // UNSUPPORTED_LANGUAGE. Language-agnostic models report an empty list, so
    // they always stay on auto.
    let language = requested_language.filter(|lang| model_languages.iter().any(|l| l == lang));
    let (task, target_language) = cpp_translation_task(
        translate_to_english,
        model_supports_translate,
        language.as_deref(),
    );

    TranscribeCppRunPlan {
        task,
        language,
        target_language,
    }
}

fn post_process_transcription_text(
    raw: String,
    settings: &AppSettings,
    custom_words_already_prompted: bool,
    output_language: &OutputLanguageEvidence,
    supported_languages: &[String],
) -> String {
    fail_open_text_transform(raw, |raw| {
        // Last-resort language evidence: confidence-gated detection from the
        // transcribed text itself, constrained to the model's languages. Only
        // consulted when it can change the outcome (built-in gated fillers or
        // Chinese script conversion).
        let output_language = match output_language {
            OutputLanguageEvidence::Unknown if wants_text_language_detection(settings) => {
                match detect_output_language(&raw, supported_languages) {
                    Some(language) => {
                        debug!("Text-based language detection resolved '{}'", language);
                        OutputLanguageEvidence::TextDetected(language)
                    }
                    None => OutputLanguageEvidence::Unknown,
                }
            }
            other => other.clone(),
        };
        apply_text_post_processing(
            raw,
            settings,
            custom_words_already_prompted,
            &output_language,
        )
    })
}

/// [`post_process_transcription_text`] with the output language already
/// resolved: never detects it from `raw`. Used for file chunks, whose
/// language is resolved once per chunk and shared by its segments.
fn post_process_resolved_text(
    raw: String,
    settings: &AppSettings,
    custom_words_already_prompted: bool,
    output_language: &OutputLanguageEvidence,
) -> String {
    fail_open_text_transform(raw, |raw| {
        apply_text_post_processing(
            raw,
            settings,
            custom_words_already_prompted,
            output_language,
        )
    })
}

/// Whether text-based language detection can change the post-processing
/// outcome (Chinese script conversion, or built-in language-gated fillers).
fn wants_text_language_detection(settings: &AppSettings) -> bool {
    settings.chinese_script != ChineseScript::AsTranscribed
        || (settings.filler_word_removal_enabled && settings.custom_filler_words.is_none())
}

fn apply_text_post_processing(
    raw: String,
    settings: &AppSettings,
    custom_words_already_prompted: bool,
    output_language: &OutputLanguageEvidence,
) -> String {
    let converts_script = settings.chinese_script != ChineseScript::AsTranscribed;
    // Convert the script before custom words so they match in the script
    // the user writes in. Only output known to be Chinese is touched, so
    // e.g. Japanese kanji are never rewritten.
    let variety = output_language
        .language()
        .and_then(ChineseVariety::from_language);
    let raw = match variety {
        Some(variety) if converts_script => {
            convert_chinese_script(&raw, variety, settings.chinese_script)
        }
        _ => raw,
    };

    let corrected = if !settings.custom_words.is_empty() && !custom_words_already_prompted {
        apply_custom_words(
            &raw,
            &settings.custom_words,
            settings.word_correction_threshold,
        )
    } else {
        raw
    };

    let without_fillers = remove_filler_words(
        &corrected,
        output_language,
        &settings.custom_filler_words,
        settings.filler_word_removal_enabled,
    );

    normalize_transcription_output(&without_fillers)
}

/// Characters to wait for before detecting the preview's language. A lone
/// Chinese character reads as Mandarin, but Japanese often opens with kanji
/// before any kana; waiting a few characters keeps those from locking in.
const PREVIEW_DETECTION_MIN_CHARS: usize = 6;

/// Converts live-preview text into the configured Chinese script as it streams.
///
/// The preview is cosmetic: the final paste re-resolves the language and
/// converts on its own, so this only has to look right while speaking.
struct PreviewScript {
    script: ChineseScript,
    variety: Option<ChineseVariety>,
    /// The language wasn't known when the stream started (auto-detect), so it
    /// is detected from the streamed text until it turns out to be Chinese.
    detect: bool,
}

impl PreviewScript {
    fn new(script: ChineseScript, output_language: &OutputLanguageEvidence) -> Self {
        let enabled = script != ChineseScript::AsTranscribed;
        Self {
            script,
            variety: output_language
                .language()
                .and_then(ChineseVariety::from_language)
                .filter(|_| enabled),
            detect: enabled && *output_language == OutputLanguageEvidence::Unknown,
        }
    }

    /// Converts the model's raw committed/tentative text. Always fed the raw
    /// text, never a previous conversion, so it stays consistent with the
    /// final conversion. Once Chinese is detected it sticks for the rest of
    /// the stream so the preview doesn't flip back and forth.
    fn convert(
        &mut self,
        committed: &str,
        tentative: &str,
        supported_languages: &[String],
    ) -> (String, String) {
        if self.variety.is_none() && self.detect {
            let text = format!("{committed}{tentative}");
            if text.chars().count() >= PREVIEW_DETECTION_MIN_CHARS {
                self.variety = detect_output_language(&text, supported_languages)
                    .as_deref()
                    .and_then(ChineseVariety::from_language);
            }
        }

        match self.variety {
            Some(variety) => (
                convert_chinese_script(committed, variety, self.script),
                convert_chinese_script(tentative, variety, self.script),
            ),
            None => (committed.to_string(), tentative.to_string()),
        }
    }
}

/// Optional text cleanup must never discard a successful model result. The
/// transform is pure and owns its input, so recovering the untouched text is
/// safe even if a bug in custom-word or filler filtering unwinds.
fn fail_open_text_transform<F>(raw: String, transform: F) -> String
where
    F: FnOnce(String) -> String,
{
    let fallback = raw.clone();
    match catch_unwind(AssertUnwindSafe(|| transform(raw))) {
        Ok(processed) => processed,
        Err(payload) => {
            error!(
                "Optional transcription text post-processing panicked: {}; using the raw transcription",
                panic_payload_message(payload.as_ref())
            );
            fallback
        }
    }
}

/// Decide a transcribe-cpp run's task + translation target from settings.
///
/// "Translate to English" only fires where the model advertises translation.
/// Unlike transcribe-rs (which forces the target to English itself when its
/// `translate` flag is set), transcribe-cpp requires an explicit
/// `target_language`: a null target defaults to the *source*, so a non-English
/// source silently becomes e.g. es→es and Canary rejects the unadvertised pair.
/// An English source is skipped entirely — en→en is not a real translation, and
/// it's reachable by default since auto-detect-less models coerce intent to "en".
///
/// Returns `(task, target_language)` ready to drop into `RunOptions`.
fn cpp_translation_task(
    translate_to_english: bool,
    model_supports_translate: bool,
    source_language: Option<&str>,
) -> (Task, Option<String>) {
    let translate_to_en =
        translate_to_english && model_supports_translate && source_language != Some("en");
    if translate_to_en {
        (Task::Translate, Some("en".to_string()))
    } else {
        (Task::Transcribe, None)
    }
}

/// Drain a stream command channel, ignoring fed audio, until the caller
/// finalizes or cancels. Used when streaming can't actually run (model not
/// loaded / not streaming-capable) so the finalize handshake still completes
/// and the caller falls back to batch transcription.
fn drain_until_finalize(rx: mpsc::Receiver<StreamCmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            StreamCmd::Feed(_) => {}
            StreamCmd::Finalize(reply) => {
                let _ = reply.send(Ok(None));
                break;
            }
            StreamCmd::Cancel => break,
        }
    }
}

/// Log the compute devices transcribe-cpp registers.
///
/// Devices are listed by a transcription worker process (see
/// [`crate::engine_supervisor`]), so GPU driver initialization never runs in
/// the app process. Listing opens the GPU, which on macOS loads ggml's Metal
/// library and compiles it when the system shader cache does not hold it yet
/// (the first launch after an install or update), so the app calls this from
/// a background thread instead of its startup path.
pub fn report_compute_devices(tm: &TranscriptionManager) {
    if transcribe_gpu_disabled_for_host() {
        warn!(
            "Windows x64 build is running under emulation on an ARM64 host; \
             disabling transcribe.cpp GPU acceleration and using CPU"
        );
    }
    match transcribe_compute_devices(&tm.engine) {
        Ok(devices) => info!(
            "transcribe-cpp initialized with {} compute device(s): [{}]",
            devices.len(),
            devices
                .iter()
                .map(|d| format!("{} ({})", d.name, d.kind))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => warn!("transcribe-cpp compute devices are unavailable: {}", e),
    }
}

/// Human-readable list of the transcribe-cpp compute devices, for the
/// `--list-devices` flag. The reported `index` is the value to pass to
/// `--device-index`. Errors if the devices could not be listed.
pub fn describe_compute_devices(tm: &TranscriptionManager) -> Result<Vec<String>> {
    Ok(transcribe_compute_devices(&tm.engine)?
        .into_iter()
        .map(|d| {
            let idx = d
                .index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".to_string());
            let vram_mb = d.memory_total / (1024 * 1024);
            format!(
                "index={} kind={} name={} vram={}MB",
                idx,
                d.kind,
                d.label(),
                vram_mb
            )
        })
        .collect())
}

/// Map Handy's whisper accelerator setting to a transcribe-cpp [`Backend`].
///
/// `Auto` lets the library pick the best device (with CPU fallback), while
/// `Cpu` forces strict CPU. `Gpu` only remains as the companion setting for an
/// exact device; without a valid exact device it has the retired generic GPU
/// state's new Auto semantics. An emulated x64 process on Windows ARM64 forces
/// strict CPU for every setting.
fn select_transcribe_backend(setting: TranscribeAcceleratorSetting) -> Backend {
    select_transcribe_backend_for_host(setting, transcribe_gpu_disabled_for_host())
}

fn select_transcribe_backend_for_host(
    setting: TranscribeAcceleratorSetting,
    gpu_disabled: bool,
) -> Backend {
    match effective_transcribe_accelerator(setting, gpu_disabled) {
        TranscribeAcceleratorSetting::Cpu => Backend::Cpu,
        TranscribeAcceleratorSetting::Auto | TranscribeAcceleratorSetting::Gpu => Backend::Auto,
    }
}

/// Resolve the user's persisted GPU choice to the device key the worker
/// should load on. Registry indices and handles are process-local, so
/// settings store a key based on the backend's stable `device_id` (falling
/// back to name for backends such as Metal that do not report one). The
/// worker falls back to automatic selection if that device is gone.
fn resolve_gpu_device(
    setting: TranscribeAcceleratorSetting,
    gpu_device: Option<&str>,
) -> Option<String> {
    if transcribe_gpu_disabled_for_host() || setting != TranscribeAcceleratorSetting::Gpu {
        return None;
    }
    gpu_device.map(str::to_string)
}

/// Apply the user's ORT accelerator preference to the transcribe-rs global.
/// Called on startup and before loading a model.
///
/// The transcribe.cpp (whisper-family) backend is no longer set here: it is
/// chosen at model-load time from [`select_transcribe_backend`], so changing the
/// accelerator only needs a model reload (see `reload_model_on_next_use`).
pub fn apply_accelerator_settings(app: &tauri::AppHandle) {
    use transcribe_rs::accel;

    let settings = get_settings(app);

    info!(
        "transcribe.cpp accelerator preference: {:?} (applied on next model load)",
        settings.transcribe_accelerator
    );

    let ort_pref = match settings.ort_accelerator {
        OrtAcceleratorSetting::Auto => accel::OrtAccelerator::Auto,
        OrtAcceleratorSetting::Cpu => accel::OrtAccelerator::CpuOnly,
        OrtAcceleratorSetting::Cuda => accel::OrtAccelerator::Cuda,
        OrtAcceleratorSetting::DirectMl => accel::OrtAccelerator::DirectMl,
        OrtAcceleratorSetting::Rocm => accel::OrtAccelerator::Rocm,
    };
    accel::set_ort_accelerator(ort_pref);
    info!("ORT accelerator set to: {}", ort_pref);
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct GpuDeviceOption {
    pub id: String,
    pub name: String,
    pub total_vram_mb: usize,
}

fn transcribe_gpu_disabled_for_host() -> bool {
    crate::utils::is_windows_x64_emulated_on_arm64()
}

fn effective_transcribe_accelerator(
    setting: TranscribeAcceleratorSetting,
    gpu_disabled: bool,
) -> TranscribeAcceleratorSetting {
    if gpu_disabled {
        TranscribeAcceleratorSetting::Cpu
    } else {
        setting
    }
}

/// The compute devices the latest worker listed. On a host without GPU
/// access every worker is CPU-only, so it lists no GPU.
fn transcribe_compute_devices(engine: &EngineSupervisor) -> Result<Vec<DeviceInfo>> {
    engine
        .devices()
        .ok_or_else(|| anyhow::anyhow!("compute devices could not be listed (see the log for why)"))
}

fn available_transcribe_accelerators(gpu_disabled: bool) -> Vec<String> {
    if gpu_disabled {
        vec!["cpu".to_string()]
    } else {
        vec!["auto".to_string(), "cpu".to_string(), "gpu".to_string()]
    }
}

fn gpu_device_options(engine: &EngineSupervisor) -> Vec<GpuDeviceOption> {
    // GPU compute devices as the latest worker listed them, so a GPU that
    // comes back is picked up. `id` is a persistent identity key, never the
    // process-local registry index. It uses the backend's device_id where
    // available and its name otherwise (Metal). `total_vram_mb` is 0 when the
    // backend does not report capacity. Empty if listing failed.
    transcribe_compute_devices(engine)
        .unwrap_or_default()
        .into_iter()
        .filter(DeviceInfo::is_gpu)
        .map(|d| GpuDeviceOption {
            id: d.key.clone(),
            name: d.label().to_string(),
            total_vram_mb: (d.memory_total / (1024 * 1024)) as usize,
        })
        .collect()
}

#[derive(Serialize, Clone, Debug, Type)]
pub struct AvailableAccelerators {
    pub transcribe: Vec<String>,
    pub ort: Vec<String>,
    pub gpu_devices: Vec<GpuDeviceOption>,
}

/// Return the accelerators available to this process on its current host.
pub fn get_available_accelerators(tm: &TranscriptionManager) -> AvailableAccelerators {
    use transcribe_rs::accel::OrtAccelerator;

    let ort_options: Vec<String> = OrtAccelerator::available()
        .into_iter()
        .map(|a| a.to_string())
        .collect();

    let transcribe_options = available_transcribe_accelerators(transcribe_gpu_disabled_for_host());

    AvailableAccelerators {
        transcribe: transcribe_options,
        ort: ort_options,
        gpu_devices: gpu_device_options(&tm.engine),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn languages(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|code| (*code).to_string()).collect()
    }

    #[test]
    fn normal_hosts_preserve_every_transcribe_accelerator_setting() {
        for setting in [
            TranscribeAcceleratorSetting::Auto,
            TranscribeAcceleratorSetting::Cpu,
            TranscribeAcceleratorSetting::Gpu,
        ] {
            assert_eq!(effective_transcribe_accelerator(setting, false), setting);
        }
        assert_eq!(
            available_transcribe_accelerators(false),
            ["auto", "cpu", "gpu"]
        );
        assert_eq!(
            select_transcribe_backend_for_host(TranscribeAcceleratorSetting::Auto, false),
            Backend::Auto
        );
        assert_eq!(
            select_transcribe_backend_for_host(TranscribeAcceleratorSetting::Cpu, false),
            Backend::Cpu
        );
        assert_eq!(
            select_transcribe_backend_for_host(TranscribeAcceleratorSetting::Gpu, false),
            Backend::Auto
        );
    }

    #[test]
    fn emulated_x64_on_arm64_forces_every_transcribe_setting_to_cpu() {
        for setting in [
            TranscribeAcceleratorSetting::Auto,
            TranscribeAcceleratorSetting::Cpu,
            TranscribeAcceleratorSetting::Gpu,
        ] {
            assert_eq!(
                effective_transcribe_accelerator(setting, true),
                TranscribeAcceleratorSetting::Cpu
            );
            assert_eq!(
                select_transcribe_backend_for_host(setting, true),
                Backend::Cpu
            );
        }
        assert_eq!(available_transcribe_accelerators(true), ["cpu"]);
    }

    #[test]
    fn optional_text_transform_falls_back_to_raw_text_after_panic() {
        let raw = "原始轉錄。".to_string();
        let result = fail_open_text_transform(raw.clone(), |_| {
            panic!("simulated optional cleanup failure")
        });

        assert_eq!(result, raw);
    }

    #[test]
    fn non_chinese_output_is_never_converted() {
        let settings = AppSettings {
            chinese_script: ChineseScript::Traditional,
            ..Default::default()
        };
        for evidence in [
            OutputLanguageEvidence::ModelDetected("ja".to_string()),
            OutputLanguageEvidence::UserSelected("en".to_string()),
            OutputLanguageEvidence::TranslatedToEnglish,
        ] {
            let result = post_process_transcription_text(
                "学校に行きます".to_string(),
                &settings,
                false,
                &evidence,
                &languages(&["zh", "ja", "en"]),
            );

            assert_eq!(result, "学校に行きます", "{evidence:?}");
        }
    }

    #[test]
    fn auto_detected_chinese_text_is_converted() {
        let settings = AppSettings {
            chinese_script: ChineseScript::Simplified,
            filler_word_removal_enabled: false,
            ..Default::default()
        };
        let result = post_process_transcription_text(
            "我們今天下午一起去學校圖書館看書，然後再去吃晚飯。".to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::Unknown,
            &languages(&["zh", "en", "ja"]),
        );

        assert_eq!(result, "我们今天下午一起去学校图书馆看书，然后再去吃晚饭。");
    }

    #[test]
    fn portuguese_transcription_does_not_use_english_ui_filler_words() {
        let settings = AppSettings {
            app_language: "en".to_string(),
            selected_language: "pt-BR".to_string(),
            ..Default::default()
        };
        let supported = languages(&["en", "pt"]);
        let evidence = resolve_output_language_evidence(&settings, Some("pt"), &supported, false);

        let result = post_process_transcription_text(
            "eu vi um carro".to_string(),
            &settings,
            false,
            &evidence,
            &supported,
        );

        assert_eq!(
            evidence,
            OutputLanguageEvidence::UserSelected("pt".to_string())
        );
        assert_eq!(result, "eu vi um carro");
    }

    #[test]
    fn norwegian_alias_is_recorded_as_user_selected_evidence() {
        let settings = AppSettings {
            selected_language: "no".to_string(),
            ..Default::default()
        };

        let evidence =
            resolve_output_language_evidence(&settings, Some("nb"), &languages(&["nb"]), false);

        assert_eq!(
            evidence,
            OutputLanguageEvidence::UserSelected("nb".to_string())
        );
    }

    #[test]
    fn auto_language_without_detection_skips_gated_filler_removal() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            ..Default::default()
        };
        let evidence =
            resolve_output_language_evidence(&settings, None, &languages(&["en", "pt"]), false);

        // Too short for a reliable text detection, so the gated "um" must
        // survive; the universal "uhm" is removed regardless.
        let result = post_process_transcription_text(
            "um uhm ok".to_string(),
            &settings,
            false,
            &evidence,
            &languages(&["en", "pt"]),
        );

        assert_eq!(evidence, OutputLanguageEvidence::Unknown);
        assert_eq!(result, "um ok");
    }

    #[test]
    fn unknown_evidence_with_confident_text_detection_removes_gated_fillers() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            ..Default::default()
        };

        let result = post_process_transcription_text(
            "um so the weather forecast said it would probably rain throughout the whole weekend"
                .to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::Unknown,
            &languages(&["en", "pt", "es", "de"]),
        );

        assert_eq!(
            result,
            "so the weather forecast said it would probably rain throughout the whole weekend"
        );
    }

    #[test]
    fn unknown_evidence_with_portuguese_text_preserves_um() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            ..Default::default()
        };

        let result = post_process_transcription_text(
            "eu vi um carro na rua ontem de manhã quando fui ao mercado".to_string(),
            &settings,
            false,
            &OutputLanguageEvidence::Unknown,
            &languages(&["en", "pt", "es", "de"]),
        );

        assert_eq!(
            result,
            "eu vi um carro na rua ontem de manhã quando fui ao mercado"
        );
    }

    #[test]
    fn model_detected_language_upgrades_unknown_evidence_only() {
        assert_eq!(
            with_model_detected_language(OutputLanguageEvidence::Unknown, Some("en".to_string())),
            OutputLanguageEvidence::ModelDetected("en".to_string())
        );
        assert_eq!(
            with_model_detected_language(OutputLanguageEvidence::Unknown, Some("auto".to_string())),
            OutputLanguageEvidence::Unknown
        );
        assert_eq!(
            with_model_detected_language(OutputLanguageEvidence::Unknown, None),
            OutputLanguageEvidence::Unknown
        );
        assert_eq!(
            with_model_detected_language(
                OutputLanguageEvidence::UserSelected("pt".to_string()),
                Some("en".to_string())
            ),
            OutputLanguageEvidence::UserSelected("pt".to_string())
        );
    }

    #[test]
    fn auto_language_uses_single_language_model_as_evidence() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            ..Default::default()
        };

        let evidence =
            resolve_output_language_evidence(&settings, None, &languages(&["en"]), false);

        assert_eq!(
            evidence,
            OutputLanguageEvidence::ModelConstrained("en".to_string())
        );
    }

    #[test]
    fn unsupported_explicit_language_uses_model_fallback_as_evidence() {
        let settings = AppSettings {
            selected_language: "pt".to_string(),
            ..Default::default()
        };

        let evidence = resolve_output_language_evidence(
            &settings,
            Some("en"),
            &languages(&["en", "de"]),
            false,
        );

        assert_eq!(
            evidence,
            OutputLanguageEvidence::ModelConstrained("en".to_string())
        );
    }

    #[test]
    fn ignored_user_language_is_not_output_evidence() {
        let settings = AppSettings {
            // Parakeet V3 ignores language hints and auto-detects even when a
            // selection from the previously active model remains persisted.
            selected_language: "en".to_string(),
            ..Default::default()
        };
        let supported = languages(&["en", "de", "pt"]);

        let evidence = resolve_output_language_evidence(&settings, None, &supported, false);
        assert_eq!(evidence, OutputLanguageEvidence::Unknown);

        let result = post_process_transcription_text(
            "eu vi um carro".to_string(),
            &settings,
            false,
            &evidence,
            &supported,
        );
        assert_eq!(result, "eu vi um carro");
    }

    #[test]
    fn unapplied_transcribe_cpp_language_is_not_output_evidence() {
        let settings = AppSettings {
            selected_language: "en".to_string(),
            ..Default::default()
        };
        let supported = languages(&[]);
        let plan = transcribe_cpp_run_plan(false, "en", &supported, false);

        assert_eq!(plan.language, None);
        assert_eq!(
            resolve_output_language_evidence(
                &settings,
                plan.language.as_deref(),
                &supported,
                false,
            ),
            OutputLanguageEvidence::Unknown
        );
    }

    #[test]
    fn translated_output_is_treated_as_english() {
        let settings = AppSettings {
            selected_language: "pt".to_string(),
            ..Default::default()
        };

        let evidence = resolve_output_language_evidence(
            &settings,
            Some("pt"),
            &languages(&["en", "pt"]),
            true,
        );

        assert_eq!(evidence, OutputLanguageEvidence::TranslatedToEnglish);
    }

    #[test]
    fn transcribe_cpp_run_plan_skips_english_translation() {
        let plan = transcribe_cpp_run_plan(true, "en", &languages(&["en", "es"]), true);

        assert!(matches!(plan.task, Task::Transcribe));
        assert_eq!(plan.language.as_deref(), Some("en"));
        assert_eq!(plan.target_language, None);
    }

    #[test]
    fn transcribe_cpp_run_plan_translates_supported_non_english() {
        let plan = transcribe_cpp_run_plan(true, "es", &languages(&["en", "es"]), true);

        assert!(matches!(plan.task, Task::Translate));
        assert_eq!(plan.language.as_deref(), Some("es"));
        assert_eq!(plan.target_language.as_deref(), Some("en"));
    }

    #[test]
    fn transcribe_cpp_run_plan_requires_model_translation_support() {
        let plan = transcribe_cpp_run_plan(true, "es", &languages(&["en", "es"]), false);

        assert!(matches!(plan.task, Task::Transcribe));
        assert_eq!(plan.language.as_deref(), Some("es"));
        assert_eq!(plan.target_language, None);
    }

    fn raw(start_ms: i64, end_ms: i64, text: &str) -> RawTimed {
        RawTimed {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    fn unchanged(text: String) -> String {
        text
    }

    #[test]
    fn chunk_limits_default_to_28_seconds_without_a_cap() {
        assert_eq!(
            compute_chunk_limits(false, None),
            ChunkLimits {
                max_chunk_ms: 28_000,
                search_window_ms: 10_000,
                hard_max_ms: None,
            }
        );
        // A cap above the default only becomes the hard limit.
        assert_eq!(
            compute_chunk_limits(false, Some(120_000)),
            ChunkLimits {
                max_chunk_ms: 28_000,
                search_window_ms: 10_000,
                hard_max_ms: Some(120_000),
            }
        );
    }

    #[test]
    fn chunk_limits_follow_gigaam_and_model_caps() {
        assert_eq!(
            compute_chunk_limits(true, Some(24_000)),
            ChunkLimits {
                max_chunk_ms: 24_000,
                search_window_ms: 10_000,
                hard_max_ms: Some(24_000),
            }
        );
        // transcribe-cpp max_audio_ms = 20 s: chunks stay a second under it.
        let (is_gigaam, cap) = transcribe_cpp_chunk_cap("whisper", 20_000);
        assert_eq!(
            compute_chunk_limits(is_gigaam, cap),
            ChunkLimits {
                max_chunk_ms: 19_000,
                search_window_ms: 9_500,
                hard_max_ms: Some(19_000),
            }
        );
        // A cap below 5 s still allows 5 s chunks (the chunker's minimum);
        // the cap is reported for callers to warn about.
        assert_eq!(
            compute_chunk_limits(false, Some(3_000)),
            ChunkLimits {
                max_chunk_ms: 5_000,
                search_window_ms: 2_500,
                hard_max_ms: Some(3_000),
            }
        );
    }

    #[test]
    fn chunk_search_window_is_at_most_half_the_chunk() {
        for (cap, window) in [
            (5_000, 2_500),
            (12_000, 6_000),
            (20_000, 10_000),
            (27_000, 10_000),
        ] {
            let limits = compute_chunk_limits(false, Some(cap));
            assert_eq!(limits.max_chunk_ms, cap);
            assert_eq!(limits.search_window_ms, window, "cap {cap}");
        }
    }

    #[test]
    fn transcribe_cpp_chunk_caps() {
        assert_eq!(transcribe_cpp_chunk_cap("whisper", 0), (false, None));
        assert_eq!(
            transcribe_cpp_chunk_cap("whisper", 30_000),
            (false, Some(29_000))
        );
        assert_eq!(transcribe_cpp_chunk_cap("gigaam", 0), (true, Some(24_000)));
        assert_eq!(
            transcribe_cpp_chunk_cap("GigaAM-v2", 20_000),
            (true, Some(19_000))
        );
        assert_eq!(
            transcribe_cpp_chunk_cap("gigaam", 60_000),
            (true, Some(24_000))
        );
        assert_eq!(transcribe_cpp_chunk_cap("moss", 800), (false, Some(0)));
    }

    #[test]
    fn onnx_chunk_caps() {
        assert_eq!(
            onnx_chunk_cap(&EngineType::Moonshine),
            (false, Some(63_000))
        );
        for engine in [
            EngineType::MoonshineStreaming,
            EngineType::Canary,
            EngineType::Cohere,
            EngineType::SenseVoice,
        ] {
            assert_eq!(onnx_chunk_cap(&engine), (false, Some(30_000)), "{engine:?}");
        }
        assert_eq!(onnx_chunk_cap(&EngineType::GigaAM), (true, Some(24_000)));
        assert_eq!(
            onnx_chunk_cap(&EngineType::Parakeet),
            (false, Some(120_000))
        );
        let (is_gigaam, cap) = onnx_chunk_cap(&EngineType::Parakeet);
        assert_eq!(compute_chunk_limits(is_gigaam, cap).max_chunk_ms, 28_000);
    }

    #[test]
    fn file_runs_request_segment_timestamps_within_the_model_capability() {
        assert_eq!(
            file_run_timestamps(TimestampKind::None),
            TimestampKind::None
        );
        assert_eq!(
            file_run_timestamps(TimestampKind::Auto),
            TimestampKind::Auto
        );
        for finer in [
            TimestampKind::Segment,
            TimestampKind::Word,
            TimestampKind::Token,
        ] {
            assert_eq!(file_run_timestamps(finer), TimestampKind::Segment);
        }
    }

    #[test]
    fn segments_are_offset_into_the_file_and_sorted() {
        let (segments, grain) = place_segments(
            "Hello there. General Kenobi.",
            "Hello there. General Kenobi.",
            vec![
                raw(2_500, 4_000, " General Kenobi."),
                raw(0, 2_000, "Hello there."),
            ],
            60_000,
            20_000,
            unchanged,
        );
        assert_eq!(grain, TimingGrain::Segment);
        assert_eq!(
            segments,
            vec![
                segment(60_000, 62_000, "Hello there."),
                segment(62_500, 64_000, "General Kenobi."),
            ]
        );
    }

    #[test]
    fn segments_are_clamped_to_the_real_chunk_length() {
        // The chunk was zero-padded to 1 s; only 600 ms are real audio.
        let (segments, grain) = place_segments(
            "one two three",
            "one two three",
            vec![
                raw(100, 400, "one two"),
                raw(450, 900, "three"),
                raw(700, 950, "pad"),
            ],
            1_000,
            600,
            unchanged,
        );
        assert_eq!(grain, TimingGrain::Segment);
        // "pad" starts past the real audio: it has no time left and is dropped.
        assert_eq!(
            segments,
            vec![
                segment(1_100, 1_400, "one two"),
                segment(1_450, 1_600, "three")
            ]
        );
    }

    #[test]
    fn empty_inverted_and_negative_segments_are_dropped() {
        let (segments, grain) = place_segments(
            "alpha beta gamma",
            "alpha beta gamma",
            vec![
                raw(0, 1_000, "alpha beta"),
                raw(1_000, 2_000, "   "),
                raw(3_000, 2_500, "inverted"),
                raw(-500, 200, "negative"),
                raw(2_000, 3_000, "gamma"),
                raw(3_000, 4_000, "um"),
            ],
            0,
            10_000,
            // Post-processing that empties a segment drops it.
            |text| {
                if text.trim() == "um" {
                    String::new()
                } else {
                    text
                }
            },
        );
        assert_eq!(grain, TimingGrain::Segment);
        assert_eq!(
            segments,
            vec![
                segment(0, 1_000, "alpha beta"),
                segment(2_000, 3_000, "gamma")
            ]
        );
    }

    #[test]
    fn segments_without_duration_get_a_short_one_inside_the_chunk() {
        let (segments, _) = place_segments(
            "first second",
            "first second",
            vec![raw(1_000, 1_000, "first"), raw(4_800, 4_800, "second")],
            10_000,
            5_000,
            unchanged,
        );
        assert_eq!(
            segments,
            vec![
                segment(11_000, 11_000 + MIN_SEGMENT_MS, "first"),
                segment(14_800, 15_000, "second"),
            ]
        );
    }

    #[test]
    fn incomplete_segments_fall_back_to_one_segment_per_chunk() {
        let text = "the quick brown fox jumps over the lazy dog";
        let (segments, grain) = place_segments(
            text,
            text,
            vec![raw(0, 1_000, "the quick"), raw(1_000, 2_000, "brown")],
            30_000,
            25_000,
            unchanged,
        );
        assert_eq!(grain, TimingGrain::Chunk);
        assert_eq!(segments, vec![segment(30_000, 55_000, text)]);

        // No rows at all (an engine without timestamps).
        let (segments, grain) = place_segments("hi", "hi", Vec::new(), 0, 1_000, unchanged);
        assert_eq!(grain, TimingGrain::Chunk);
        assert_eq!(segments, vec![segment(0, 1_000, "hi")]);

        // Rows whose text all post-processes away.
        let (segments, grain) =
            place_segments("hi", "hi", vec![raw(0, 500, "hi")], 0, 1_000, |_| {
                String::new()
            });
        assert_eq!(grain, TimingGrain::Chunk);
        assert_eq!(segments, vec![segment(0, 1_000, "hi")]);
    }

    #[test]
    fn an_empty_chunk_has_no_segments() {
        let (segments, grain) = place_segments("", "um", vec![raw(0, 500, "um")], 0, 1_000, |_| {
            String::new()
        });
        assert!(segments.is_empty());
        assert_eq!(grain, TimingGrain::Segment);
    }

    #[test]
    fn segment_coverage_ignores_whitespace() {
        let text = "aaaa bbbb   cccc dddd";
        assert!(segments_cover_text(
            &[raw(0, 1, "aaaa"), raw(1, 2, "bbbb")],
            text
        ));
        assert!(!segments_cover_text(&[raw(0, 1, "aaaa bbb")], text));
        assert!(!segments_cover_text(&[raw(0, 1, "x")], ""));
    }

    #[test]
    fn unknown_chunks_inherit_the_job_language_evidence() {
        let mut stored = None;
        // The first confident chunk is stored and no detection runs.
        let evidence = resolve_chunk_evidence(
            OutputLanguageEvidence::ModelDetected("de".to_string()),
            &mut stored,
            || panic!("detection is not needed"),
        );
        assert_eq!(
            evidence,
            OutputLanguageEvidence::ModelDetected("de".to_string())
        );
        assert_eq!(stored, Some(evidence.clone()));

        // A later chunk without evidence reuses it, without detection.
        let evidence = resolve_chunk_evidence(OutputLanguageEvidence::Unknown, &mut stored, || {
            panic!("detection is not needed")
        });
        assert_eq!(
            evidence,
            OutputLanguageEvidence::ModelDetected("de".to_string())
        );

        // A chunk with its own evidence keeps it; the stored one stays.
        let evidence = resolve_chunk_evidence(
            OutputLanguageEvidence::TranslatedToEnglish,
            &mut stored,
            || None,
        );
        assert_eq!(evidence, OutputLanguageEvidence::TranslatedToEnglish);
        assert_eq!(
            stored,
            Some(OutputLanguageEvidence::ModelDetected("de".to_string()))
        );
    }

    #[test]
    fn unknown_first_chunks_fall_back_to_text_detection() {
        let mut stored = None;
        let evidence =
            resolve_chunk_evidence(OutputLanguageEvidence::Unknown, &mut stored, || None);
        assert_eq!(evidence, OutputLanguageEvidence::Unknown);
        assert_eq!(stored, None);

        let evidence = resolve_chunk_evidence(OutputLanguageEvidence::Unknown, &mut stored, || {
            Some("pt".to_string())
        });
        assert_eq!(
            evidence,
            OutputLanguageEvidence::TextDetected("pt".to_string())
        );
        assert_eq!(stored, Some(evidence));
    }

    #[test]
    fn resolved_post_processing_never_detects_the_language() {
        let settings = AppSettings {
            selected_language: "auto".to_string(),
            ..Default::default()
        };
        assert!(wants_text_language_detection(&settings));
        let text =
            "um so the weather forecast said it would probably rain throughout the whole weekend";
        // Gated fillers need evidence; a segment never detects on its own.
        assert_eq!(
            post_process_resolved_text(
                text.to_string(),
                &settings,
                false,
                &OutputLanguageEvidence::Unknown
            ),
            text
        );
        assert_eq!(
            post_process_resolved_text(
                text.to_string(),
                &settings,
                false,
                &OutputLanguageEvidence::TextDetected("en".to_string())
            ),
            post_process_transcription_text(
                text.to_string(),
                &settings,
                false,
                &OutputLanguageEvidence::Unknown,
                &languages(&["en", "pt", "es", "de"]),
            )
        );
    }

    #[test]
    fn dictation_hold_releases_its_count_on_drop() {
        let holds = Arc::new(AtomicUsize::new(0));
        holds.fetch_add(2, Ordering::SeqCst);
        let first = DictationHold(Arc::clone(&holds));
        let second = DictationHold(Arc::clone(&holds));
        drop(first);
        assert_eq!(holds.load(Ordering::SeqCst), 1);
        drop(second);
        assert_eq!(holds.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn onnx_in_use_mark_is_cleared_even_by_a_panic() {
        let in_use = Arc::new(AtomicBool::new(true));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _in_use = OnnxInUse(Arc::clone(&in_use));
            panic!("engine panicked");
        }));
        assert!(result.is_err());
        assert!(!in_use.load(Ordering::SeqCst));

        in_use.store(true, Ordering::SeqCst);
        drop(OnnxInUse(Arc::clone(&in_use)));
        assert!(!in_use.load(Ordering::SeqCst));
    }

    #[test]
    fn only_engine_cancels_count_as_cancels() {
        assert!(is_engine_cancel(&EngineError::Cancelled.into()));
        assert!(!is_engine_cancel(
            &EngineError::Failed("worker crashed".to_string()).into()
        ));
        assert!(!is_engine_cancel(&anyhow::anyhow!("cancelled")));
    }

    #[test]
    fn chunk_errors_read_as_english() {
        assert_eq!(
            ChunkError::ModelChanged {
                job_model: "a".to_string(),
                current_model: "b".to_string(),
            }
            .to_string(),
            "the transcription model changed from 'a' to 'b'"
        );
        assert_eq!(
            ChunkError::Failed(anyhow::anyhow!("boom").context("chunk")).to_string(),
            "chunk: boom"
        );
        assert_eq!(
            ChunkError::Cancelled.to_string(),
            "file transcription was cancelled"
        );
    }

    fn loading_flag() -> (Arc<Mutex<bool>>, Arc<Condvar>) {
        (Arc::new(Mutex::new(false)), Arc::new(Condvar::new()))
    }

    #[test]
    fn claim_load_leaves_a_loaded_model_alone() {
        let (is_loading, condvar) = loading_flag();
        assert!(claim_load_unless_loaded(&is_loading, &condvar, || true).is_none());
        assert!(!*is_loading.lock().unwrap());
    }

    /// A model unloaded under a dictation that is starting is loaded by its
    /// transcribe() rather than failing it; the flag is held for the load.
    #[test]
    fn claim_load_claims_the_flag_for_an_unloaded_model() {
        let (is_loading, condvar) = loading_flag();
        let guard = claim_load_unless_loaded(&is_loading, &condvar, || false);
        assert!(guard.is_some());
        assert!(*is_loading.lock().unwrap());
        drop(guard);
        assert!(!*is_loading.lock().unwrap());
    }

    /// Callers racing for an unloaded model load it once: the rest wait for
    /// that load and then find the model loaded.
    #[test]
    fn claim_load_loads_once_for_concurrent_callers() {
        let (is_loading, condvar) = loading_flag();
        let loaded = Arc::new(AtomicBool::new(false));
        let loads = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (is_loading, condvar) = (Arc::clone(&is_loading), Arc::clone(&condvar));
                let (loaded, loads) = (Arc::clone(&loaded), Arc::clone(&loads));
                thread::spawn(move || {
                    let claimed = claim_load_unless_loaded(&is_loading, &condvar, || {
                        loaded.load(Ordering::SeqCst)
                    });
                    if let Some(_loading) = claimed {
                        loads.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        loaded.store(true, Ordering::SeqCst);
                    }
                    assert!(loaded.load(Ordering::SeqCst));
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(!*is_loading.lock().unwrap());
    }

    /// A load already in progress is waited for, not duplicated.
    #[test]
    fn claim_load_waits_for_a_load_in_progress() {
        let (is_loading, condvar) = loading_flag();
        let loaded = Arc::new(AtomicBool::new(false));
        let guard = claim_load_unless_loaded(&is_loading, &condvar, || false).unwrap();
        let loader = {
            let loaded = Arc::clone(&loaded);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                loaded.store(true, Ordering::SeqCst);
                drop(guard);
            })
        };
        let claimed =
            claim_load_unless_loaded(&is_loading, &condvar, || loaded.load(Ordering::SeqCst));
        assert!(claimed.is_none());
        loader.join().unwrap();
    }
}

impl Drop for TranscriptionManager {
    fn drop(&mut self) {
        // Skip shutdown unless this is the very last clone. TranscriptionManager
        // is cloned by initiate_model_load() and the watcher thread — those
        // clones dropping must not kill the watcher. The watcher thread holds
        // its own clone, so onnx's strong_count is always >= 2 while the
        // watcher is alive. When it reaches 1, only this instance remains
        // and we can safely shut down.
        if Arc::strong_count(&self.onnx) > 1 {
            return;
        }

        // Signal the watcher thread to shutdown
        self.shutdown_signal.store(true, Ordering::Relaxed);

        // Wait for the thread to finish gracefully.
        // Use match instead of unwrap to avoid panicking if the mutex is
        // poisoned — a panic inside Drop calls abort().
        let mut guard = match self.watcher_handle.lock() {
            Ok(g) => g,
            Err(e) => {
                warn!("Recovered poisoned watcher_handle mutex during TranscriptionManager drop — a panic occurred earlier this session");
                e.into_inner()
            }
        };
        if let Some(handle) = guard.take() {
            if let Err(e) = handle.join() {
                warn!("Failed to join idle watcher thread: {:?}", e);
            } else {
                debug!("Idle watcher thread joined successfully");
            }
        }
    }
}
