//! Parent side: [`EngineSupervisor`], the single owner of the transcribe.cpp
//! worker process.
//!
//! One owner thread works through a command queue and holds the only worker
//! slot, the loaded model's spec and any active stream, so "at most one
//! model worker alive" holds by construction. Callers' commands wait their
//! turn; waiting in the queue (e.g. behind a model load) is never a timeout.
//! Only cancels ([`EngineSupervisor::cancel`] and
//! [`EngineSupervisor::cancel_work`]) and device probes bypass the queue: a
//! probe runs its own short-lived, model-less worker on the caller's thread,
//! so a hung GPU driver can't hold up dictation behind it.
//!
//! A worker that will only use the CPU is started CPU-only, so it never
//! registers a GPU backend. One rule decides: the spec it loads asks for
//! [`Backend::Cpu`] (the user's choice, or a fallback after a GPU failure),
//! or the host must not use the GPU at all.

use super::protocol::{
    encode_message, read_message, DeviceInfo, DeviceSelector, LoadedInfo, Request, Response,
};
use super::worker::LOG_LINE_PREFIX;
use super::{
    WorkToken, WorkerOptions, CPU_ONLY_FLAG, LOG_LEVEL_ENV, LOW_PRIORITY_FLAG, WORKER_FLAG,
};
use log::{debug, error, info, warn, Level};
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use transcribe_cpp::{
    Backend, DeviceType, RunOptions, StreamOptions, StreamText, StreamUpdate, Transcript,
};

/// Spawn, backend init and device listing. Listing first opens the GPU,
/// which on macOS compiles ggml's Metal library when the system shader cache
/// does not hold it yet (~15 s after an install or update).
const HELLO_TIMEOUT: Duration = Duration::from_secs(60);
/// Model loads stay generous: a slow load is slow, not hung (#1841).
const LOAD_TIMEOUT: Duration = Duration::from_secs(180);
/// Floor for stream calls that do no decoding (begin, reset).
const CALL_FLOOR: Duration = Duration::from_secs(30);
/// How long a worker gets to exit after its stdin closes before it's killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Native stderr lines kept for crash reports (a `GGML_ASSERT` message lands
/// here right before an abort).
const STDERR_TAIL_LINES: usize = 64;
const SAMPLE_RATE: f64 = 16_000.0;

/// How long decoding work may take before its worker counts as hung, by the
/// device the model runs on. On a GPU a hang is recovered from (retried on
/// CPU), so it should be caught soon. On CPU a hang is never retried, so the
/// deadline only decides when to give up: a slow machine must not be
/// mistaken for a hung one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Deadlines {
    /// Floor for a stream feed or finalize, so sub-second chunks and
    /// first-use shader compiles are not mistaken for hangs.
    call_floor: Duration,
    /// Floor for a batch run. A run has a fixed cost however short the
    /// clip: Whisper always encodes a full 30 s window, which on CPU takes
    /// seconds even for a 1 s clip.
    run_floor: Duration,
    /// Work on N seconds of audio may take up to this many times N.
    audio_factor: f64,
}

const GPU_DEADLINES: Deadlines = Deadlines {
    call_floor: Duration::from_secs(30),
    run_floor: Duration::from_secs(60),
    audio_factor: 10.0,
};

/// A single CPU feed or finalize can take well over 10 s, and a large model
/// on an older CPU runs slower than real time.
const CPU_DEADLINES: Deadlines = Deadlines {
    call_floor: Duration::from_secs(120),
    run_floor: Duration::from_secs(300),
    audio_factor: 20.0,
};

impl Deadlines {
    fn for_device(on_gpu: bool) -> Self {
        if on_gpu {
            GPU_DEADLINES
        } else {
            CPU_DEADLINES
        }
    }

    /// Deadline for stream work on `samples` of 16 kHz audio.
    fn audio(&self, samples: usize) -> Duration {
        self.call_floor.max(Duration::from_secs_f64(
            samples as f64 / SAMPLE_RATE * self.audio_factor,
        ))
    }

    /// Deadline for a batch run on `samples` of 16 kHz audio.
    fn run(&self, samples: usize) -> Duration {
        self.run_floor.max(self.audio(samples))
    }
}

fn ms_to_samples(ms: i64) -> usize {
    (ms.max(0) as usize) * (SAMPLE_RATE as usize / 1000)
}

/// What to load. The supervisor keeps it so a dead worker can be replaced
/// transparently.
#[derive(Debug, Clone)]
pub struct LoadSpec {
    pub path: PathBuf,
    pub backend: Backend,
    pub device: DeviceSelector,
    /// Free memory the GPU must report for the model to load there, checked
    /// each time a worker starts for it (see [`gpu_fits`]). With less, the
    /// model loads in a CPU-only worker instead and its
    /// [`LoadedInfo::gpu_memory_fallback`] is set. `None` never checks; nor
    /// does a CPU or pinned spec. Room kept for another model only on the
    /// load itself is [`EngineSupervisor::load_reserving`]'s, never part of
    /// the spec.
    pub min_gpu_free_bytes: Option<u64>,
}

impl LoadSpec {
    /// An explicitly chosen device (`--device-index`) never falls back to
    /// CPU: its failures are reported, not hidden.
    fn pinned(&self) -> bool {
        matches!(self.device, DeviceSelector::Index(_))
    }

    fn cpu_only(&self) -> bool {
        self.backend == Backend::Cpu
    }

    fn cpu(&self) -> Self {
        Self {
            path: self.path.clone(),
            backend: Backend::Cpu,
            device: DeviceSelector::Auto,
            min_gpu_free_bytes: None,
        }
    }
}

/// Whether a model that needs `need` bytes of free GPU memory fits on the
/// GPU `selector` would load on, by the free memory `devices` last reported.
/// A key names that GPU (an unknown key falls back to automatic selection,
/// as in the worker); automatic selection takes the first discrete GPU, else
/// the first integrated one. An explicit index is never second-guessed, and
/// without a target GPU or a reported free amount there is nothing to judge.
fn gpu_fits(devices: &[DeviceInfo], selector: &DeviceSelector, need: u64) -> bool {
    let first = |kind: DeviceType| devices.iter().find(|device| device.device_type == kind);
    let auto = || first(DeviceType::Gpu).or_else(|| first(DeviceType::Igpu));
    let target = match selector {
        DeviceSelector::Index(_) => return true,
        DeviceSelector::Key(key) => devices
            .iter()
            .find(|device| device.is_gpu() && &device.key == key)
            .or_else(auto),
        DeviceSelector::Auto => auto(),
    };
    match target {
        Some(device) if device.memory_free > 0 => device.memory_free >= need,
        _ => true,
    }
}

/// The free GPU memory a new worker for `spec` must find to load it there:
/// the spec's own need plus `reserve_bytes` (the load's room for another
/// model, 0 for a worker that replaces a lost one). `None` checks nothing: a
/// spec without a need, a pinned device, or a worker that won't use the GPU.
fn gpu_need(spec: &LoadSpec, reserve_bytes: u64, cpu_only_worker: bool) -> Option<u64> {
    if cpu_only_worker || spec.pinned() {
        return None;
    }
    spec.min_gpu_free_bytes
        .map(|need| need.saturating_add(reserve_bytes))
}

#[derive(Debug)]
pub enum EngineError {
    /// [`EngineSupervisor::cancel`] (or, for scoped work,
    /// [`EngineSupervisor::cancel_work`]) stopped the work.
    Cancelled,
    /// No model, a transcribe.cpp error, or a worker failure that could not
    /// be recovered.
    Failed(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Cancelled => f.write_str("transcription was cancelled"),
            EngineError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for EngineError {}

/// The result of one stream feed, handed to the stream's callback.
pub struct StreamProgress {
    pub update: StreamUpdate,
    /// Present only when the committed or tentative text changed.
    pub text: Option<StreamText>,
    /// Time from sending the frame to the worker until its answer.
    pub elapsed: Duration,
}

pub struct Finalized {
    pub update: StreamUpdate,
    pub text: StreamText,
    /// The model's detected language, when asked for.
    pub language: Option<String>,
    pub elapsed: Duration,
}

type Reply<T> = mpsc::Sender<T>;
type OnProgress = Box<dyn FnMut(StreamProgress) + Send>;

enum Command {
    Load {
        spec: LoadSpec,
        /// See [`EngineSupervisor::load_reserving`].
        reserve_bytes: u64,
        reply: Reply<Result<LoadedInfo, EngineError>>,
    },
    Unload(Reply<()>),
    Transcribe {
        pcm: Vec<f32>,
        run: RunOptions,
        epoch: u64,
        /// Set for scoped work (a file transcription); see
        /// [`EngineSupervisor::cancel_work`].
        token: Option<WorkToken>,
        /// Set for scoped work that yields to interrupts: the interrupt
        /// count its caller read; see
        /// [`EngineSupervisor::transcribe_interruptible`].
        interrupts: Option<u64>,
        reply: Reply<Result<Transcript, EngineError>>,
    },
    StreamBegin {
        active: ActiveStream,
        run: RunOptions,
        stream: StreamOptions,
        reply: Reply<Result<(), EngineError>>,
    },
    Feed {
        id: u64,
        pcm: Vec<f32>,
    },
    Finalize {
        id: u64,
        want_language: bool,
        epoch: u64,
        reply: Reply<Result<Option<Finalized>, EngineError>>,
    },
    StreamReset {
        id: u64,
    },
}

/// State readable without going through the queue.
#[derive(Default)]
struct Shared {
    /// Whether this host may use a GPU at all. When not, every worker is
    /// CPU-only, device probes included.
    gpu_allowed: bool,
    /// How workers are started.
    options: WorkerOptions,
    /// A worker with GPU backends crashed or hung (in backend init, device
    /// listing, model load or inference). Models load in CPU-only workers,
    /// and devices are not listed again, until [`EngineSupervisor::retry_gpu`].
    gpu_unavailable: AtomicBool,
    /// The loaded model (logically: it stays loaded while its worker is
    /// replaced after a crash or cancel).
    loaded: Mutex<Option<LoadedInfo>>,
    /// The compute devices as last listed by a worker; `None` until one has.
    devices: Mutex<Option<Vec<DeviceInfo>>>,
    /// Held for the length of a device probe, so concurrent callers wait for
    /// the one probe instead of starting their own.
    probing: Mutex<()>,
    /// Bumped by every [`EngineSupervisor::cancel`]. Work queued before the
    /// bump counts as cancelled.
    cancel_epoch: AtomicU64,
    /// The call the owner is waiting on, if any. A cancel takes it and kills
    /// its worker.
    in_flight: Mutex<Option<InFlight>>,
    /// Bumped by every [`EngineSupervisor::interrupt_scoped_work`] under the
    /// `in_flight` lock, whether or not a call was in flight, so an
    /// interruptible call that has not started yet sees it too.
    interrupts: AtomicU64,
    /// Bumped by every [`EngineSupervisor::unload`] as it is called, so the
    /// model reads as unloaded at once, even while the unload waits its turn.
    unloads_requested: AtomicU64,
    next_id: AtomicU64,
}

impl Shared {
    /// Start a model-less worker just to list devices. If it may use the GPU
    /// and crashes or hangs doing so, the GPU is marked unavailable.
    fn probe_devices(&self) {
        let cpu_only = !self.gpu_allowed;
        match self.hello(cpu_only, true) {
            Ok(worker) => drop(worker),
            Err(failure) => {
                error!(
                    "Could not start a transcription worker to list compute devices: {}",
                    failure
                );
                if failure.worker_lost() && !cpu_only {
                    self.mark_gpu_unavailable(format!(
                        "a worker with GPU backends {failure} while listing compute devices"
                    ));
                }
            }
        }
    }

    /// Start a worker and say hello, publishing its device list if it lists
    /// one.
    fn hello(&self, cpu_only: bool, list_devices: bool) -> Result<Worker, Failure> {
        let mut worker =
            Worker::spawn(cpu_only, self.options.low_priority).map_err(Failure::Spawn)?;
        match worker.call(&Request::Hello { list_devices }, None, HELLO_TIMEOUT)? {
            Response::Hello { devices } => {
                if let Some(devices) = devices {
                    self.set_devices(devices);
                }
                Ok(worker)
            }
            other => Err(unexpected(&other)),
        }
    }

    fn set_devices(&self, new: Vec<DeviceInfo>) {
        let mut current = lock(&self.devices);
        if let Some(old) = &*current {
            let identity = |d: &DeviceInfo| (d.key.clone(), d.index);
            if !old.iter().map(identity).eq(new.iter().map(identity)) {
                info!(
                    "transcribe-cpp compute devices changed: [{}]",
                    new.iter()
                        .map(|d| format!("{} ({})", d.name, d.kind))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        *current = Some(new);
    }

    fn gpu_unavailable(&self) -> bool {
        self.gpu_unavailable.load(Ordering::Acquire)
    }

    /// `reason` is logged when this changes the state.
    fn mark_gpu_unavailable(&self, reason: impl fmt::Display) {
        if !self.gpu_unavailable.swap(true, Ordering::AcqRel) {
            warn!("Transcription GPU marked unavailable: {}", reason);
        }
    }

    fn epoch(&self) -> u64 {
        self.cancel_epoch.load(Ordering::SeqCst)
    }

    fn cancelled_since(&self, epoch: u64) -> bool {
        self.epoch() != epoch
    }

    fn interrupts(&self) -> u64 {
        self.interrupts.load(Ordering::SeqCst)
    }

    /// Whether work must not start: a cancel came after `epoch`, its
    /// `token` was cancelled, or, for work that yields to interrupts, an
    /// interrupt came after its caller read `interrupts`.
    fn stopped(&self, epoch: u64, token: Option<&WorkToken>, interrupts: Option<u64>) -> bool {
        self.cancelled_since(epoch)
            || token.is_some_and(WorkToken::is_cancelled)
            || interrupts.is_some_and(|seen| self.interrupts() != seen)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

struct InFlight {
    what: &'static str,
    control: Arc<Control>,
    /// The scoped work's token, if the call carries one.
    token: Option<WorkToken>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Owns the one transcribe.cpp worker slot. Cheap to clone.
#[derive(Clone)]
pub struct EngineSupervisor {
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
}

impl EngineSupervisor {
    /// Start the owner thread. No worker runs until something needs one.
    /// Without `gpu_allowed`, every worker is CPU-only, so no GPU driver is
    /// ever loaded.
    pub fn new(gpu_allowed: bool) -> Self {
        Self::new_with(gpu_allowed, WorkerOptions::default())
    }

    /// [`new`](Self::new), starting workers as `options` says.
    pub fn new_with(gpu_allowed: bool, options: WorkerOptions) -> Self {
        #[cfg(all(unix, not(test)))]
        record_launched_exe();
        let (commands, queue) = mpsc::channel();
        let shared = Arc::new(Shared {
            gpu_allowed,
            options,
            ..Shared::default()
        });
        let owner = Owner {
            shared: Arc::clone(&shared),
            worker: None,
            spec: None,
            stream: None,
            unloads_done: 0,
        };
        thread::Builder::new()
            .name("transcribe-engine".into())
            .spawn(move || owner.run(queue))
            .expect("failed to start the transcription engine thread");
        Self { commands, shared }
    }

    fn ask<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Option<T> {
        let (reply, answer) = mpsc::channel();
        self.commands.send(command(reply)).ok()?;
        answer.recv().ok()
    }

    /// Load a model, replacing the current one. The old worker has fully
    /// exited before the new one starts, so two models are never held at
    /// once.
    pub fn load(&self, spec: LoadSpec) -> Result<LoadedInfo, EngineError> {
        self.load_reserving(spec, 0)
    }

    /// [`load`](Self::load), keeping `reserve_bytes` of GPU memory free
    /// beyond the spec's [`LoadSpec::min_gpu_free_bytes`] for another model
    /// that will go to the same GPU but isn't there yet. Only this load
    /// checks for it: a worker started later for the model (after a crash,
    /// cancel or interrupt) checks the spec's own need, because by then the
    /// GPU's free memory already counts that other model.
    pub fn load_reserving(
        &self,
        spec: LoadSpec,
        reserve_bytes: u64,
    ) -> Result<LoadedInfo, EngineError> {
        self.ask(|reply| Command::Load {
            spec,
            reserve_bytes,
            reply,
        })
        .unwrap_or_else(|| Err(engine_stopped()))
    }

    /// Unload the model, without waiting. It reads as unloaded at once and
    /// the unload is queued ahead of anything requested later, so a new
    /// dictation queues a fresh load behind it and is never affected by it.
    /// [`Unloading::wait`] returns once the worker has exited.
    pub fn unload(&self) -> Unloading {
        let (reply, exited) = mpsc::channel();
        {
            let mut loaded = lock(&self.shared.loaded);
            self.shared.unloads_requested.fetch_add(1, Ordering::SeqCst);
            *loaded = None;
            if self.commands.send(Command::Unload(reply)).is_err() {
                return Unloading(None);
            }
        }
        Unloading(Some(exited))
    }

    /// An explicit transcribe.cpp accelerator or GPU device change: forget
    /// the GPU failure, if any, so the next load may use (and list) the GPU
    /// again. A new failure sets the flag again.
    pub fn retry_gpu(&self, reason: &str) {
        if self.shared.gpu_unavailable.swap(false, Ordering::AcqRel) {
            info!("Transcription GPU no longer marked unavailable: {}", reason);
        }
    }

    /// The loaded model. A snapshot; never waits on the queue.
    pub fn loaded(&self) -> Option<LoadedInfo> {
        lock(&self.shared.loaded).clone()
    }

    /// Transcribe 16 kHz mono PCM with the loaded model. A worker crash or
    /// hang is retried once in a fresh worker (on CPU if it was on the GPU).
    pub fn transcribe(&self, pcm: Vec<f32>, run: RunOptions) -> Result<Transcript, EngineError> {
        self.submit_transcribe(pcm, run, None, None)
    }

    /// [`transcribe`](Self::transcribe) as scoped work: besides
    /// [`cancel`](Self::cancel), [`cancel_work`](Self::cancel_work) with the
    /// same `token` stops it. Cancelling the token never affects other work.
    pub fn transcribe_scoped(
        &self,
        pcm: Vec<f32>,
        run: RunOptions,
        token: &WorkToken,
    ) -> Result<Transcript, EngineError> {
        self.submit_transcribe(pcm, run, Some(token.clone()), None)
    }

    /// [`transcribe_scoped`](Self::transcribe_scoped) that yields to every
    /// [`interrupt_scoped_work`](Self::interrupt_scoped_work) after its
    /// caller read `interrupts_seen` from [`interrupts`](Self::interrupts),
    /// not only to one that finds it running: a call interrupted before it
    /// started (queued, or while its worker was still being replaced)
    /// returns `Cancelled` without running.
    pub fn transcribe_interruptible(
        &self,
        pcm: Vec<f32>,
        run: RunOptions,
        token: &WorkToken,
        interrupts_seen: u64,
    ) -> Result<Transcript, EngineError> {
        self.submit_transcribe(pcm, run, Some(token.clone()), Some(interrupts_seen))
    }

    fn submit_transcribe(
        &self,
        pcm: Vec<f32>,
        run: RunOptions,
        token: Option<WorkToken>,
        interrupts: Option<u64>,
    ) -> Result<Transcript, EngineError> {
        let epoch = self.shared.epoch();
        self.ask(|reply| Command::Transcribe {
            pcm,
            run,
            epoch,
            token,
            interrupts,
            reply,
        })
        .unwrap_or_else(|| Err(engine_stopped()))
    }

    /// Begin a live stream on the loaded model. `on_progress` runs on the
    /// owner thread after every feed, so it must not block or call back into
    /// the supervisor.
    pub fn start_stream(
        &self,
        run: RunOptions,
        stream: StreamOptions,
        on_progress: impl FnMut(StreamProgress) + Send + 'static,
    ) -> Result<StreamHandle, EngineError> {
        let id = self.shared.next_id();
        // The stream is one piece of cancellable work: a cancel from here on
        // stops its feeds and its finalize.
        let epoch = self.shared.epoch();
        let closed = Arc::new(AtomicBool::new(false));
        self.ask(|reply| Command::StreamBegin {
            active: ActiveStream {
                id,
                epoch,
                closed: Arc::clone(&closed),
                on_progress: Box::new(on_progress),
                pending_ms: 0,
            },
            run,
            stream,
            reply,
        })
        .unwrap_or_else(|| Err(engine_stopped()))?;
        Ok(StreamHandle {
            id,
            epoch,
            closed,
            commands: self.commands.clone(),
            finished: false,
        })
    }

    /// Stop the transcription or stream in progress, and any queued behind
    /// it. Bypasses the queue. The worker doing the work is killed, so the
    /// next use starts a fresh one and reloads the model. Model loads are
    /// never cancelled.
    pub fn cancel(&self) {
        self.shared.cancel_epoch.fetch_add(1, Ordering::SeqCst);
        // Taking the call tells the owner its worker was killed.
        if let Some(call) = lock(&self.shared.in_flight).take() {
            info!(
                "Cancelling the {} by stopping its worker (pid {})",
                call.what, call.control.pid
            );
            call.control.kill();
        }
    }

    /// Stop the scoped work submitted with `token`, queued or in progress,
    /// and nothing else: unlike [`cancel`](Self::cancel) it leaves the cancel
    /// epoch alone, so dictation is never affected. A call in progress is
    /// stopped by killing its worker (the next use starts a fresh one and
    /// reloads the model); queued calls see the token and never start.
    pub fn cancel_work(&self, token: &WorkToken) {
        token.cancel();
        // Checked under the lock `call_work` publishes under, after the
        // token is set: either it sees the token and sends nothing, or this
        // sees its call and kills its worker.
        let ours = lock(&self.shared.in_flight)
            .take_if(|call| call.token.as_ref().is_some_and(|t| t.same_as(token)));
        if let Some(call) = ours {
            info!(
                "Cancelling the scoped {} by stopping its worker (pid {})",
                call.what, call.control.pid
            );
            call.control.kill();
        }
    }

    /// Stop the scoped (file) call in progress, if any, without cancelling
    /// its token or bumping the cancel epoch: its caller gets `Cancelled`
    /// with a live token and may submit again. Unscoped work is never
    /// touched. Returns whether a call was stopped.
    ///
    /// The call is stopped by killing its worker, which is neither a crash
    /// nor a GPU failure: the next use starts a fresh worker for the model.
    /// An interruptible call that has not started yet never starts (see
    /// [`transcribe_interruptible`](Self::transcribe_interruptible)).
    pub fn interrupt_scoped_work(&self) -> bool {
        let call = {
            let mut in_flight = lock(&self.shared.in_flight);
            // Counted under the lock `call_work` publishes under: either it
            // sees the count and sends nothing, or this sees its call.
            self.shared.interrupts.fetch_add(1, Ordering::SeqCst);
            in_flight.take_if(|call| call.token.is_some())
        };
        match call {
            Some(call) => {
                info!(
                    "Interrupting the scoped {} by stopping its worker (pid {})",
                    call.what, call.control.pid
                );
                call.control.kill();
                true
            }
            None => false,
        }
    }

    /// How many times [`interrupt_scoped_work`](Self::interrupt_scoped_work)
    /// has been called, for
    /// [`transcribe_interruptible`](Self::transcribe_interruptible).
    pub fn interrupts(&self) -> u64 {
        self.shared.interrupts()
    }

    /// The compute devices, as listed by the latest worker. If no worker has
    /// listed them yet, starts a short-lived worker on this thread to list
    /// them. It runs beside any model worker, which it leaves alone. Not
    /// while the GPU is marked unavailable: that probe would crash or hang
    /// the same way.
    pub fn devices(&self) -> Option<Vec<DeviceInfo>> {
        let _probing = lock(&self.shared.probing);
        if lock(&self.shared.devices).is_none() && !self.shared.gpu_unavailable() {
            self.shared.probe_devices();
        }
        lock(&self.shared.devices).clone()
    }
}

/// An unload in progress; see [`EngineSupervisor::unload`].
pub struct Unloading(Option<mpsc::Receiver<()>>);

impl Unloading {
    /// Wait until the worker has exited (e.g. before deleting the model file).
    pub fn wait(self) {
        if let Some(exited) = self.0 {
            let _ = exited.recv();
        }
    }
}

/// A live stream in the worker. Feeds are queued without waiting, in order
/// with finalize. Dropping the handle without finalizing resets the stream.
pub struct StreamHandle {
    id: u64,
    /// Cancel epoch when the stream started.
    epoch: u64,
    /// Set when the handle is dropped; shared with the owner.
    closed: Arc<AtomicBool>,
    commands: mpsc::Sender<Command>,
    finished: bool,
}

impl StreamHandle {
    pub fn feed(&self, pcm: Vec<f32>) {
        let _ = self.commands.send(Command::Feed { id: self.id, pcm });
    }

    /// Flush the stream and return its final text. `Ok(None)` if the stream
    /// was lost or failed; fall back to batch transcription then.
    /// `Err(Cancelled)` if [`EngineSupervisor::cancel`] stopped it.
    pub fn finalize(mut self, want_language: bool) -> Result<Option<Finalized>, EngineError> {
        self.finished = true;
        let (reply, answer) = mpsc::channel();
        let command = Command::Finalize {
            id: self.id,
            want_language,
            epoch: self.epoch,
            reply,
        };
        if self.commands.send(command).is_err() {
            return Ok(None);
        }
        answer.recv().unwrap_or(Ok(None))
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        if !self.finished {
            // Queued feeds for this stream are skipped from here on.
            self.closed.store(true, Ordering::Release);
            let _ = self.commands.send(Command::StreamReset { id: self.id });
        }
    }
}

fn engine_stopped() -> EngineError {
    EngineError::Failed("the transcription engine has stopped".to_string())
}

fn not_loaded() -> EngineError {
    EngineError::Failed("no transcription model is loaded".to_string())
}

/// How a call to the worker failed.
#[derive(Debug)]
enum Failure {
    /// The worker process could not be started.
    Spawn(io::Error),
    /// transcribe.cpp returned an error; the worker is fine.
    Remote(String),
    /// The worker exited or crashed. Carries exit status and its last
    /// native output.
    Crashed(String),
    /// The worker did not answer within the deadline and was killed.
    Hung(Duration),
    Cancelled,
}

impl Failure {
    /// Whether the worker is gone.
    fn worker_lost(&self) -> bool {
        matches!(self, Failure::Crashed(_) | Failure::Hung(_))
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Spawn(e) => write!(f, "failed to start transcription worker: {e}"),
            Failure::Remote(message) => f.write_str(message),
            Failure::Crashed(detail) => write!(f, "transcription worker crashed ({detail})"),
            Failure::Hung(t) => write!(f, "transcription worker did not respond within {t:?}"),
            Failure::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl From<Failure> for EngineError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Cancelled => EngineError::Cancelled,
            other => EngineError::Failed(other.to_string()),
        }
    }
}

fn unexpected(response: &Response) -> Failure {
    Failure::Remote(format!(
        "unexpected transcription worker response: {response:?}"
    ))
}

// --- Owner thread ---------------------------------------------------------

struct ActiveStream {
    id: u64,
    /// Cancel epoch when the stream started; a later cancel stops it.
    epoch: u64,
    /// Set when the caller drops its handle.
    closed: Arc<AtomicBool>,
    on_progress: OnProgress,
    /// Audio received but not yet committed, which finalize must still
    /// decode; sizes the feed and finalize deadlines.
    pending_ms: i64,
}

struct Owner {
    shared: Arc<Shared>,
    /// The single worker slot.
    worker: Option<Worker>,
    /// The model the app asked for. Outlives its worker: after a crash,
    /// hang or cancel the next use starts a fresh worker for it.
    spec: Option<LoadSpec>,
    stream: Option<ActiveStream>,
    /// Unload commands processed, against [`Shared::unloads_requested`].
    unloads_done: u64,
}

impl Owner {
    fn run(mut self, queue: mpsc::Receiver<Command>) {
        while let Ok(command) = queue.recv() {
            match command {
                Command::Load {
                    spec,
                    reserve_bytes,
                    reply,
                } => {
                    let _ = reply.send(self.load(spec, reserve_bytes));
                }
                Command::Unload(reply) => {
                    self.unload();
                    self.unloads_done += 1;
                    let _ = reply.send(());
                }
                Command::Transcribe {
                    pcm,
                    run,
                    epoch,
                    token,
                    interrupts,
                    reply,
                } => {
                    let _ =
                        reply.send(self.transcribe(&pcm, &run, epoch, token.as_ref(), interrupts));
                }
                Command::StreamBegin {
                    active,
                    run,
                    stream,
                    reply,
                } => {
                    let _ = reply.send(self.stream_begin(active, &run, &stream));
                }
                Command::Feed { id, pcm } => self.feed(id, &pcm),
                Command::Finalize {
                    id,
                    want_language,
                    epoch,
                    reply,
                } => {
                    let _ = reply.send(self.finalize(id, want_language, epoch));
                }
                Command::StreamReset { id } => {
                    if self.stream.as_ref().is_some_and(|s| s.id == id) {
                        self.end_stream();
                    }
                }
            }
        }
        // Every handle is gone.
        self.unload();
    }

    /// `reserve_bytes` counts for this start only; the kept spec never holds
    /// it (see [`EngineSupervisor::load_reserving`]).
    fn load(&mut self, spec: LoadSpec, reserve_bytes: u64) -> Result<LoadedInfo, EngineError> {
        self.unload();
        self.spec = Some(spec);
        let result = self.start(reserve_bytes);
        if result.is_err() {
            self.spec = None;
        }
        result
    }

    fn unload(&mut self) {
        // The worker's exit ends any stream; no need to reset it first.
        self.stream = None;
        if let Some(worker) = self.worker.take() {
            let started = Instant::now();
            let pid = worker.control.pid;
            drop(worker);
            debug!(
                "Transcription worker (pid {}) stopped in {}ms",
                pid,
                started.elapsed().as_millis()
            );
        }
        self.spec = None;
        *lock(&self.shared.loaded) = None;
    }

    /// The spec a fresh worker should load: CPU while the GPU is marked
    /// unavailable, unless the device was chosen explicitly.
    fn effective_spec(&self, spec: &LoadSpec) -> LoadSpec {
        if spec.pinned() || spec.cpu_only() || !self.shared.gpu_unavailable() {
            return spec.clone();
        }
        info!("Loading the transcription model on CPU: the GPU failed earlier");
        spec.cpu()
    }

    /// Whether a worker for `spec` registers only the CPU backends.
    fn cpu_only_worker(&self, spec: &LoadSpec) -> bool {
        spec.cpu_only() || !self.shared.gpu_allowed
    }

    /// Start a worker for `spec` and load it there. A worker that may use the
    /// GPU lists the devices in its hello, which keeps the list current (a
    /// GPU that comes back is just noticed). A CPU-only worker would only
    /// see the CPU, so it doesn't list.
    ///
    /// A spec with [`LoadSpec::min_gpu_free_bytes`] is checked against the
    /// devices this hello just listed, with `reserve_bytes` on top (see
    /// [`gpu_need`]): when the GPU has too little free memory, the model
    /// loads in a CPU-only worker instead.
    fn start_worker(
        &self,
        spec: &LoadSpec,
        reserve_bytes: u64,
    ) -> Result<(Worker, LoadedInfo), Failure> {
        let cpu_only = self.cpu_only_worker(spec);
        let mut worker = self.shared.hello(cpu_only, !cpu_only)?;
        if let Some(need) = gpu_need(spec, reserve_bytes, cpu_only) {
            let fits = lock(&self.shared.devices)
                .as_deref()
                .is_none_or(|devices| gpu_fits(devices, &spec.device, need));
            if !fits {
                info!(
                    "Not enough free GPU memory for '{}' ({} MiB needed); loading it on the CPU",
                    spec.path.display(),
                    need / (1024 * 1024)
                );
                drop(worker);
                let mut worker = self.shared.hello(true, false)?;
                let mut info = worker.load(&spec.cpu())?;
                info.gpu_memory_fallback = true;
                if let Some(loaded) = worker.info.as_mut() {
                    loaded.gpu_memory_fallback = true;
                }
                return Ok((worker, info));
            }
        }
        let info = worker.load(spec)?;
        Ok((worker, info))
    }

    /// Start a worker and load the current spec in it, keeping
    /// `reserve_bytes` of GPU memory free beside it. If a worker with GPU
    /// backends is lost on the way (backend init, device listing or the
    /// load), mark the GPU unavailable and retry once in a CPU-only worker.
    fn start(&mut self, reserve_bytes: u64) -> Result<LoadedInfo, EngineError> {
        let spec = self.spec.clone().ok_or_else(not_loaded)?;
        let started = Instant::now();
        let wanted = self.effective_spec(&spec);
        let (worker, info) = match self.start_worker(&wanted, reserve_bytes) {
            Ok(started) => started,
            Err(failure)
                if failure.worker_lost() && !self.cpu_only_worker(&wanted) && !wanted.pinned() =>
            {
                self.shared.mark_gpu_unavailable(format!(
                    "a worker for '{}' with GPU backends {}",
                    wanted.path.display(),
                    failure
                ));
                warn!("Retrying in a CPU-only worker");
                self.start_worker(&wanted.cpu(), 0)?
            }
            Err(failure) => return Err(failure.into()),
        };
        debug!(
            "Worker (pid {}) spawned and loaded the model in {}ms",
            worker.control.pid,
            started.elapsed().as_millis()
        );
        self.publish_loaded(&info);
        self.worker = Some(worker);
        Ok(info)
    }

    /// Report the model loaded, unless an unload was requested since and is
    /// still waiting in the queue.
    fn publish_loaded(&self, info: &LoadedInfo) {
        let mut loaded = lock(&self.shared.loaded);
        if self.shared.unloads_requested.load(Ordering::SeqCst) == self.unloads_done {
            *loaded = Some(info.clone());
        }
    }

    /// Make sure the slot holds a worker with the model loaded, starting a
    /// fresh one if the last one died or was cancelled.
    fn ensure_worker(&mut self) -> Result<(), EngineError> {
        if self.worker.is_some() {
            return Ok(());
        }
        if self.spec.is_none() {
            return Err(not_loaded());
        }
        info!("Starting a fresh transcription worker for the loaded model");
        // No reserve: the free memory the new worker's hello reports already
        // counts whatever the load reserved room for.
        if let Err(e) = self.start(0) {
            // The model can't be brought back; report it unloaded so the
            // next use loads it from scratch (and reports why it can't).
            self.spec = None;
            *lock(&self.shared.loaded) = None;
            return Err(e);
        }
        Ok(())
    }

    /// Drop a worker that crashed or hung (its handle already logged the
    /// details and native output). If it was on a GPU, mark the GPU
    /// unavailable so the next worker uses CPU. Returns whether it was.
    fn worker_lost(&mut self, during: &str, failure: &Failure) -> bool {
        // Dropping the handle reaps the process.
        let info = self.worker.take().and_then(|worker| worker.info.clone());
        let pinned = self.spec.as_ref().is_some_and(LoadSpec::pinned);
        match info {
            Some(info) if info.on_gpu => {
                if !pinned {
                    self.shared.mark_gpu_unavailable(format!(
                        "{failure} during {during} on GPU '{}' ({})",
                        info.device, info.backend
                    ));
                }
                true
            }
            _ => false,
        }
    }

    /// Deadlines for the device the current worker's model runs on.
    fn deadlines(&self) -> Deadlines {
        let on_gpu = self
            .worker
            .as_ref()
            .and_then(|worker| worker.info.as_ref())
            .is_some_and(|info| info.on_gpu);
        Deadlines::for_device(on_gpu)
    }

    /// A call that [`EngineSupervisor::cancel`] (or, for scoped work,
    /// [`EngineSupervisor::cancel_work`] with its `token`) can stop. Returns
    /// `Cancelled` if a cancel came after `epoch`, or the token was
    /// cancelled, whatever the worker said; with `interrupts`, also without
    /// sending anything if an interrupt came after its caller read them.
    fn call_work(
        &mut self,
        request: &Request,
        pcm: Option<&[f32]>,
        deadline: Duration,
        epoch: u64,
        token: Option<&WorkToken>,
        interrupts: Option<u64>,
    ) -> Result<Response, Failure> {
        let worker = self.worker.as_mut().expect("caller ensured a worker");
        let frame = encode_request(request, pcm)?;
        let sent = {
            // Check, send and publish under the lock cancel() takes after
            // bumping the epoch (cancel_work() after setting the token, and
            // interrupt_scoped_work() as it counts): either this sees the
            // cancel and sends nothing, or the cancel sees the call and
            // kills its worker.
            let mut in_flight = lock(&self.shared.in_flight);
            if self.shared.stopped(epoch, token, interrupts) {
                return Err(Failure::Cancelled);
            }
            let sent = worker.control.send(frame);
            *in_flight = Some(InFlight {
                what: request_name(request),
                control: Arc::clone(&worker.control),
                token: token.cloned(),
            });
            sent
        };
        let result = if sent {
            worker.wait(request, deadline)
        } else {
            Err(worker.died())
        };
        // Gone if cancel() or cancel_work() took the call, which means it
        // killed the worker (possibly just after it answered).
        let killed = lock(&self.shared.in_flight).take().is_none();
        if killed
            || self.shared.cancelled_since(epoch)
            || token.is_some_and(WorkToken::is_cancelled)
        {
            if killed || result.as_ref().is_err_and(Failure::worker_lost) {
                // The next use starts a fresh worker.
                self.worker = None;
            }
            return Err(Failure::Cancelled);
        }
        result
    }

    /// Retry once after a crash or hang, on CPU if it happened on the GPU.
    /// Not after a hang on CPU, and never for an explicitly chosen device.
    fn transcribe(
        &mut self,
        pcm: &[f32],
        run: &RunOptions,
        epoch: u64,
        token: Option<&WorkToken>,
        interrupts: Option<u64>,
    ) -> Result<Transcript, EngineError> {
        // A stream left open would make the worker refuse the run.
        self.end_stream();
        let request = Request::Run {
            options: run.clone(),
        };
        let mut retried = false;
        loop {
            // Also before a respawn: work that yields to an interrupt must
            // not load its model beside the dictation that interrupted it.
            if self.shared.stopped(epoch, token, interrupts) {
                return Err(EngineError::Cancelled);
            }
            // A respawn here runs under the load deadline; the run's own
            // deadline starts only once the model is loaded.
            self.ensure_worker()?;
            let deadline = self.deadlines().run(pcm.len());
            match self.call_work(&request, Some(pcm), deadline, epoch, token, interrupts) {
                Ok(Response::Transcript(transcript)) => return Ok(transcript),
                Ok(other) => return Err(unexpected(&other).into()),
                Err(failure) if failure.worker_lost() => {
                    let on_gpu = self.worker_lost("transcription", &failure);
                    let pinned = self.spec.as_ref().is_some_and(LoadSpec::pinned);
                    let hung_on_cpu = matches!(failure, Failure::Hung(_)) && !on_gpu;
                    if retried || pinned || hung_on_cpu {
                        return Err(failure.into());
                    }
                    warn!("Retrying the transcription in a fresh worker");
                    retried = true;
                }
                Err(failure) => return Err(failure.into()),
            }
        }
    }

    fn stream_begin(
        &mut self,
        active: ActiveStream,
        run: &RunOptions,
        stream: &StreamOptions,
    ) -> Result<(), EngineError> {
        self.end_stream();
        if self.shared.cancelled_since(active.epoch) {
            return Err(EngineError::Cancelled);
        }
        self.ensure_worker()?;
        let worker = self.worker.as_mut().expect("worker was just ensured");
        let request = Request::StreamBegin {
            run: run.clone(),
            stream: stream.clone(),
        };
        match worker.call(&request, None, CALL_FLOOR) {
            Ok(Response::Ok) => {
                self.stream = Some(active);
                Ok(())
            }
            Ok(other) => Err(unexpected(&other).into()),
            Err(failure) => {
                if failure.worker_lost() {
                    self.worker_lost("stream start", &failure);
                }
                Err(failure.into())
            }
        }
    }

    /// Feeds never respawn the worker (the stream state would be lost); a
    /// lost stream just drops the rest of its frames and finalizes to
    /// `None`, and the caller falls back to batch. Feeds are cancellable like
    /// any other dictation work.
    fn feed(&mut self, id: u64, pcm: &[f32]) {
        let Some(stream) = self.stream.as_ref().filter(|s| s.id == id) else {
            return;
        };
        // Skipped once the caller dropped its handle or cancelled.
        if stream.closed.load(Ordering::Acquire)
            || self.shared.cancelled_since(stream.epoch)
            || self.worker.is_none()
        {
            return;
        }
        let epoch = stream.epoch;
        let deadline = self
            .deadlines()
            .audio(pcm.len() + ms_to_samples(stream.pending_ms));
        let started = Instant::now();
        match self.call_work(&Request::Feed, Some(pcm), deadline, epoch, None, None) {
            Ok(Response::Fed { update, text }) => {
                let Some(stream) = self.stream.as_mut() else {
                    return;
                };
                stream.pending_ms = pending_ms(&update);
                let progress = StreamProgress {
                    update,
                    text,
                    elapsed: started.elapsed(),
                };
                if catch_unwind(AssertUnwindSafe(|| (stream.on_progress)(progress))).is_err() {
                    error!("Live transcription callback panicked");
                }
            }
            Ok(other) => warn!("Stream feed failed: {}", unexpected(&other)),
            Err(Failure::Remote(message)) => warn!("Stream feed failed: {}", message),
            // Later feeds are skipped, and finalize reports the cancel.
            Err(Failure::Cancelled) => {}
            Err(failure) => {
                self.stream = None;
                self.worker_lost("stream feed", &failure);
            }
        }
    }

    fn finalize(
        &mut self,
        id: u64,
        want_language: bool,
        epoch: u64,
    ) -> Result<Option<Finalized>, EngineError> {
        // Checked first: a stream lost to a cancel must report the cancel,
        // not "no result", which would start a batch run.
        if self.shared.cancelled_since(epoch) {
            if self.stream.as_ref().is_some_and(|s| s.id == id) {
                self.end_stream();
            }
            return Err(EngineError::Cancelled);
        }
        let Some(stream) = self.stream.take_if(|s| s.id == id) else {
            // Lost earlier (or replaced): fall back to batch.
            return Ok(None);
        };
        if self.worker.is_none() {
            return Ok(None);
        }
        let deadline = self.deadlines().audio(ms_to_samples(stream.pending_ms));
        let started = Instant::now();
        match self.call_work(
            &Request::Finalize { want_language },
            None,
            deadline,
            epoch,
            None,
            None,
        ) {
            Ok(Response::Finalized {
                update,
                text,
                language,
            }) => Ok(Some(Finalized {
                update,
                text,
                language,
                elapsed: started.elapsed(),
            })),
            Ok(other) => {
                error!("Stream finalize failed: {}", unexpected(&other));
                Ok(None)
            }
            Err(Failure::Cancelled) => {
                // The finalize may never have reached the worker.
                self.reset_worker_stream();
                Err(EngineError::Cancelled)
            }
            Err(Failure::Remote(message)) => {
                error!("Stream finalize failed: {}", message);
                Ok(None)
            }
            Err(failure) => {
                self.worker_lost("stream finalize", &failure);
                Ok(None)
            }
        }
    }

    fn end_stream(&mut self) {
        if self.stream.take().is_some() {
            self.reset_worker_stream();
        }
    }

    fn reset_worker_stream(&mut self) {
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        match worker.call(&Request::StreamReset, None, CALL_FLOOR) {
            Ok(_) => {}
            // Expected when the stream already ended in the worker.
            Err(Failure::Remote(message)) => debug!("Stream reset: {}", message),
            Err(failure) => {
                self.worker_lost("stream reset", &failure);
            }
        }
    }
}

fn pending_ms(update: &StreamUpdate) -> i64 {
    update
        .buffered_ms
        .max(update.input_received_ms - update.audio_committed_ms)
        .max(0)
}

// --- Worker process handle ------------------------------------------------

/// The parts of a worker that other threads touch: its stdin queue and the
/// process (to kill it). Every write goes through the one writer thread, so
/// a cancel can never interleave with a large frame.
struct Control {
    pid: u32,
    child: Mutex<Child>,
    stdin: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
}

impl Control {
    fn send(&self, frame: Vec<u8>) -> bool {
        lock(&self.stdin)
            .as_ref()
            .is_some_and(|stdin| stdin.send(frame).is_ok())
    }

    /// Closing stdin tells the worker to exit (it ends on EOF).
    fn close_stdin(&self) {
        lock(&self.stdin).take();
    }

    fn kill(&self) {
        let _ = lock(&self.child).kill();
    }
}

/// One worker process, owned by the supervisor's thread. Strict
/// request/response. Dropping it closes the worker's stdin, gives it
/// [`SHUTDOWN_GRACE`] to exit, kills it otherwise, and reaps it: once
/// dropped, the worker is gone.
struct Worker {
    control: Arc<Control>,
    responses: mpsc::Receiver<Response>,
    stderr_done: mpsc::Receiver<()>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Set once a model is loaded.
    info: Option<LoadedInfo>,
}

impl Worker {
    /// Start a worker process. A `cpu_only` worker never registers a GPU
    /// backend; a `low_priority` one runs below normal priority.
    fn spawn(cpu_only: bool, low_priority: bool) -> io::Result<Self> {
        // Under `cargo test` the current exe is the test harness; tests point
        // this at the built app instead.
        #[cfg(test)]
        let exe = PathBuf::from(
            std::env::var_os("HANDY_TRANSCRIBE_WORKER_EXE")
                .expect("set HANDY_TRANSCRIBE_WORKER_EXE"),
        );
        #[cfg(not(test))]
        let exe = worker_exe()?;
        let mut child = Self::command(exe, cpu_only, low_priority).spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let (stdin_tx, frames) = mpsc::channel();
        let (response_tx, responses) = mpsc::channel();
        let (stderr_done_tx, stderr_done) = mpsc::channel();
        #[cfg(test)]
        LIVE_WORKERS.fetch_add(1, Ordering::SeqCst);
        // Built before the threads so a failure below still reaps the child.
        let worker = Worker {
            control: Arc::new(Control {
                pid,
                child: Mutex::new(child),
                stdin: Mutex::new(Some(stdin_tx)),
            }),
            responses,
            stderr_done,
            stderr_tail: Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES))),
            info: None,
        };

        thread::Builder::new()
            .name("transcribe-worker-in".into())
            .spawn(move || write_frames(stdin, frames))?;
        thread::Builder::new()
            .name("transcribe-worker-out".into())
            .spawn(move || read_responses(stdout, response_tx))?;
        // Always drain stderr: a full pipe would block the worker.
        let tail = Arc::clone(&worker.stderr_tail);
        thread::Builder::new()
            .name("transcribe-worker-err".into())
            .spawn(move || {
                // Split on raw bytes: native code may write non-UTF-8 (e.g. a
                // path in the Windows ANSI code page), and `lines()` would end
                // the loop there, losing every later line, abort message included.
                for line in BufReader::new(stderr).split(b'\n') {
                    let Ok(line) = line else { break };
                    let line = String::from_utf8_lossy(&line);
                    let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
                    forward_worker_log(&line);
                    let mut tail = lock(&tail);
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
                let _ = stderr_done_tx.send(());
            })?;

        debug!(
            "Started {}transcription worker (pid {})",
            if cpu_only { "CPU-only " } else { "" },
            pid
        );
        Ok(worker)
    }

    /// The command line that starts a worker from `exe`.
    fn command(exe: PathBuf, cpu_only: bool, low_priority: bool) -> ProcessCommand {
        let mut command = ProcessCommand::new(exe);
        #[cfg(target_os = "linux")]
        if let Some(arg0) = std::env::args_os().next() {
            // Keep `ps` showing Handy's own path rather than /proc/self/exe.
            use std::os::unix::process::CommandExt;
            command.arg0(arg0);
        }
        command.arg(WORKER_FLAG);
        if cpu_only {
            command.arg(CPU_ONLY_FLAG);
        }
        // Windows sets the priority class at creation instead, below.
        #[cfg(unix)]
        if low_priority {
            command.arg(LOW_PRIORITY_FLAG);
        }
        command
            .env(LOG_LEVEL_ENV, log::max_level().as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
            let priority = if low_priority {
                BELOW_NORMAL_PRIORITY_CLASS
            } else {
                0
            };
            command.creation_flags(CREATE_NO_WINDOW | priority);
        }
        command
    }

    fn load(&mut self, spec: &LoadSpec) -> Result<LoadedInfo, Failure> {
        let request = Request::Load {
            path: spec.path.clone(),
            backend: spec.backend,
            device: spec.device.clone(),
        };
        match self.call(&request, None, LOAD_TIMEOUT)? {
            Response::Loaded(info) => {
                self.info = Some(info.clone());
                Ok(info)
            }
            other => Err(unexpected(&other)),
        }
    }

    /// Send one request and wait up to `deadline` for its response. Hitting
    /// the deadline kills the worker; after any `Crashed`/`Hung` the handle
    /// is dead and should be dropped.
    fn call(
        &mut self,
        request: &Request,
        pcm: Option<&[f32]>,
        deadline: Duration,
    ) -> Result<Response, Failure> {
        if !self.control.send(encode_request(request, pcm)?) {
            return Err(self.died());
        }
        self.wait(request, deadline)
    }

    /// Wait for the response to `request`, already sent.
    fn wait(&mut self, request: &Request, deadline: Duration) -> Result<Response, Failure> {
        match self.responses.recv_timeout(deadline) {
            Ok(Response::Error(message)) => Err(Failure::Remote(message)),
            Ok(response) => Ok(response),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.control.kill();
                let tail = self.native_tail();
                error!(
                    "Transcription worker (pid {}) did not answer {} within {:?}; killed it{}",
                    self.control.pid,
                    request_name(request),
                    deadline,
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!(". Native output before the hang:\n{}", tail.join("\n"))
                    }
                );
                Err(Failure::Hung(deadline))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.died()),
        }
    }

    /// The worker's stdout closed: collect its exit status and last output.
    fn died(&mut self) -> Failure {
        // Let the stderr thread catch the final lines (the abort message).
        let _ = self.stderr_done.recv_timeout(Duration::from_secs(1));
        let status = {
            let mut child = lock(&self.control.child);
            match wait_exit(&mut child, Duration::from_secs(1)) {
                Some(status) => status.to_string(),
                None => {
                    let _ = child.kill();
                    "unresponsive after closing its output; killed".to_string()
                }
            }
        };
        let tail = self.native_tail();
        if tail.is_empty() {
            error!(
                "Transcription worker (pid {}) died: {}",
                self.control.pid, status
            );
        } else {
            error!(
                "Transcription worker (pid {}) died: {}. Native output before exit:\n{}",
                self.control.pid,
                status,
                tail.join("\n")
            );
        }
        Failure::Crashed(match tail.last() {
            Some(last) => format!("{status}: {last}"),
            None => status,
        })
    }

    /// Recent raw native output (worker log lines are excluded; they were
    /// already re-logged).
    fn native_tail(&self) -> Vec<String> {
        lock(&self.stderr_tail)
            .iter()
            .filter(|line| !line.starts_with(LOG_LINE_PREFIX))
            .cloned()
            .collect()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.control.close_stdin();
        let mut child = lock(&self.control.child);
        if wait_exit(&mut child, SHUTDOWN_GRACE).is_none() {
            warn!(
                "Transcription worker (pid {}) did not exit within {:?}; killing it",
                self.control.pid, SHUTDOWN_GRACE
            );
            let _ = child.kill();
            let _ = child.wait();
        }
        #[cfg(test)]
        LIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Wait up to `timeout` for `child` to exit. `None` if it is still running
/// (or its status can't be read).
fn wait_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => return None,
        }
    }
}

/// Worker processes started and not yet reaped. Every worker is reaped when
/// its handle drops, so this is exactly the worker processes still alive.
#[cfg(test)]
static LIVE_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The file Handy was started from, with its identity (device and inode),
/// recorded at startup; see [`worker_exe`]. `None` if either couldn't be
/// read, which skips the check.
#[cfg(all(unix, not(test)))]
static LAUNCHED_EXE: std::sync::OnceLock<Option<(PathBuf, (u64, u64))>> =
    std::sync::OnceLock::new();

#[cfg(all(unix, not(test)))]
fn file_id(path: &std::path::Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Must run before an upgrade can replace the file, so at startup.
#[cfg(all(unix, not(test)))]
fn record_launched_exe() {
    LAUNCHED_EXE.get_or_init(|| {
        std::env::current_exe()
            .and_then(|path| {
                let id = file_id(&path)?;
                Ok((path, id))
            })
            .inspect_err(|e| warn!("Could not identify Handy's executable: {}", e))
            .ok()
    });
}

/// The executable a worker runs: always this very binary, so the two agree
/// on the protocol and on transcribe.cpp's backend libraries. Upgrading or
/// moving Handy while it runs replaces or removes the file it started from,
/// and a worker started from there would be the new version (macOS), or this
/// binary loading the new version's backend libraries (Linux). Only a
/// restart fixes that, so refuse to start one and say so. Windows locks a
/// running executable against replacement. On Linux the worker runs from
/// `/proc/self/exe`, which is this binary even once its file is replaced.
#[cfg(not(test))]
fn worker_exe() -> io::Result<PathBuf> {
    #[cfg(unix)]
    if let Some((path, launched)) = LAUNCHED_EXE.get().and_then(Option::as_ref) {
        if file_id(path).ok().as_ref() != Some(launched) {
            return Err(io::Error::other(
                "Handy was updated or moved while it was running; restart Handy",
            ));
        }
    }
    #[cfg(target_os = "linux")]
    return Ok(PathBuf::from("/proc/self/exe"));
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe()
}

fn encode_request(request: &Request, pcm: Option<&[f32]>) -> Result<Vec<u8>, Failure> {
    encode_message(request, pcm)
        .map_err(|e| Failure::Remote(format!("failed to encode request: {e}")))
}

fn write_frames(mut stdin: ChildStdin, frames: mpsc::Receiver<Vec<u8>>) {
    // Ends when the handle closes stdin (dropping it here sends EOF) or the
    // worker is gone.
    for frame in frames {
        if stdin
            .write_all(&frame)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            break;
        }
    }
}

fn read_responses(stdout: ChildStdout, responses: mpsc::Sender<Response>) {
    let mut stdout = BufReader::new(stdout);
    loop {
        match read_message::<Response>(&mut stdout) {
            Ok(Some((response, _))) => {
                if responses.send(response).is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                error!("Transcription worker protocol error: {}", e);
                break;
            }
        }
    }
}

fn request_name(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "hello",
        Request::Load { .. } => "model load",
        Request::Run { .. } => "transcription",
        Request::StreamBegin { .. } => "stream begin",
        Request::Feed => "stream feed",
        Request::Finalize { .. } => "stream finalize",
        Request::StreamReset => "stream reset",
    }
}

/// Re-log a worker stderr line in the parent. Structured lines keep their
/// level; anything else is raw native output.
fn forward_worker_log(line: &str) {
    const TARGET: &str = "transcribe_worker";
    if let Some(rest) = line.strip_prefix(LOG_LINE_PREFIX) {
        let mut parts = rest.splitn(3, '\t');
        if let (Some(level), Some(target), Some(message)) =
            (parts.next(), parts.next(), parts.next())
        {
            let level = level.parse::<Level>().unwrap_or(Level::Info);
            log::log!(target: TARGET, level, "[{}] {}", target, message);
            return;
        }
    }
    log::info!(target: TARGET, "{}", line);
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn deadlines_scale_with_the_audio_above_a_floor() {
        let gpu = Deadlines::for_device(true);
        assert_eq!(gpu, GPU_DEADLINES);
        assert_eq!(gpu.audio(0), gpu.call_floor);
        assert_eq!(gpu.audio(16_000 / 2), gpu.call_floor);
        assert_eq!(gpu.audio(16_000 * 5), Duration::from_secs(50));
        assert_eq!(gpu.run(16_000), gpu.run_floor);
        assert_eq!(gpu.run(16_000 * 10), Duration::from_secs(100));

        let cpu = Deadlines::for_device(false);
        assert_eq!(cpu, CPU_DEADLINES);
        assert_eq!(cpu.audio(16_000), cpu.call_floor);
        assert_eq!(cpu.audio(16_000 * 10), Duration::from_secs(200));
        assert_eq!(cpu.run(16_000 * 5), cpu.run_floor);
        assert_eq!(cpu.run(16_000 * 60), Duration::from_secs(1200));
        // CPU is never stricter than GPU.
        for secs in [0, 1, 5, 30, 120, 600] {
            assert!(cpu.audio(secs * 16_000) >= gpu.audio(secs * 16_000));
            assert!(cpu.run(secs * 16_000) >= gpu.run(secs * 16_000));
        }

        assert_eq!(ms_to_samples(1500), 24_000);
        assert_eq!(ms_to_samples(-5), 0);
    }

    #[test]
    fn low_priority_flag_reaches_the_worker_command() {
        let args = |cpu_only: bool, low_priority: bool| -> Vec<String> {
            Worker::command(PathBuf::from("handy"), cpu_only, low_priority)
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(args(false, false), [WORKER_FLAG]);
        assert_eq!(args(true, false), [WORKER_FLAG, CPU_ONLY_FLAG]);
        #[cfg(unix)]
        assert_eq!(
            args(true, true),
            [WORKER_FLAG, CPU_ONLY_FLAG, LOW_PRIORITY_FLAG]
        );
        // Windows lowers the priority class instead of passing a flag.
        #[cfg(windows)]
        assert_eq!(args(true, true), [WORKER_FLAG, CPU_ONLY_FLAG]);
    }

    #[test]
    fn work_tokens_compare_by_identity() {
        let token = WorkToken::new();
        let other = WorkToken::new();
        assert!(token.same_as(&token.clone()));
        assert!(!token.same_as(&other));
        token.clone().cancel();
        assert!(token.is_cancelled());
        assert!(!other.is_cancelled());
    }

    /// A scoped cancel never counts as a global one, so dictation work
    /// submitted before or after it is unaffected. No worker is started.
    #[test]
    fn cancel_work_sets_the_token_and_leaves_the_epoch_alone() {
        let engine = EngineSupervisor::new(false);
        let token = WorkToken::new();
        let epoch = engine.shared.epoch();
        engine.cancel_work(&token);
        assert!(token.is_cancelled());
        assert_eq!(engine.shared.epoch(), epoch);
        assert!(!engine.shared.cancelled_since(epoch));
    }

    /// `cancel_work` kills the in-flight call only when it carries the same
    /// token: never an unscoped (dictation) call, nor another job's.
    #[cfg(unix)]
    #[test]
    fn cancel_work_kills_only_a_call_with_its_token() {
        // A stand-in for a worker process.
        let child = ProcessCommand::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let control = Arc::new(Control {
            pid: child.id(),
            child: Mutex::new(child),
            stdin: Mutex::new(None),
        });
        let engine = EngineSupervisor::new(false);
        let ours = WorkToken::new();
        let publish = |token: Option<WorkToken>| {
            *lock(&engine.shared.in_flight) = Some(InFlight {
                what: "transcription",
                control: Arc::clone(&control),
                token,
            });
        };

        publish(None);
        engine.cancel_work(&ours);
        assert!(lock(&engine.shared.in_flight).is_some());

        publish(Some(WorkToken::new()));
        engine.cancel_work(&ours);
        assert!(lock(&engine.shared.in_flight).is_some());
        assert!(lock(&control.child).try_wait().unwrap().is_none());

        publish(Some(ours.clone()));
        engine.cancel_work(&ours);
        assert!(lock(&engine.shared.in_flight).is_none());
        let status = lock(&control.child).wait().unwrap();
        assert!(!status.success(), "the stand-in worker was killed");
        assert_eq!(engine.shared.epoch(), 0);
    }

    /// `interrupt_scoped_work` stops only a scoped call, and leaves its
    /// token and the cancel epoch alone, so the caller can tell it from a
    /// cancel and submit again.
    #[cfg(unix)]
    #[test]
    fn interrupt_scoped_work_stops_only_a_scoped_call() {
        let stand_in = || {
            let child = ProcessCommand::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep");
            Arc::new(Control {
                pid: child.id(),
                child: Mutex::new(child),
                stdin: Mutex::new(None),
            })
        };
        let engine = EngineSupervisor::new(false);
        let publish = |control: &Arc<Control>, token: Option<WorkToken>| {
            *lock(&engine.shared.in_flight) = Some(InFlight {
                what: "transcription",
                control: Arc::clone(control),
                token,
            });
        };

        assert!(!engine.interrupt_scoped_work(), "nothing in flight");

        let dictation = stand_in();
        publish(&dictation, None);
        assert!(!engine.interrupt_scoped_work());
        assert!(lock(&engine.shared.in_flight).is_some());
        assert!(lock(&dictation.child).try_wait().unwrap().is_none());
        dictation.kill();
        let _ = lock(&dictation.child).wait();

        let chunk = stand_in();
        let token = WorkToken::new();
        publish(&chunk, Some(token.clone()));
        assert!(engine.interrupt_scoped_work());
        assert!(lock(&engine.shared.in_flight).is_none());
        let status = lock(&chunk.child).wait().unwrap();
        assert!(!status.success(), "the stand-in worker was killed");
        assert!(!token.is_cancelled());
        assert_eq!(engine.shared.epoch(), 0);
    }

    /// An interrupt that finds nothing in flight (the chunk's worker is
    /// still being replaced, say) still stops an interruptible call whose
    /// caller read the count before it: the call returns `Cancelled`
    /// without starting a worker, and leaves the token and the cancel epoch
    /// alone. A call with the count read since, or a plain scoped call, is
    /// not affected. No worker is started.
    #[test]
    fn an_interrupt_stops_an_interruptible_call_that_has_not_started() {
        let engine = EngineSupervisor::new(false);
        let token = WorkToken::new();
        let pcm = || vec![0.0; 16_000];
        let seen = engine.interrupts();
        assert!(!engine.shared.stopped(0, Some(&token), Some(seen)));

        assert!(!engine.interrupt_scoped_work(), "nothing in flight");
        assert_eq!(engine.interrupts(), seen + 1);
        assert!(engine.shared.stopped(0, Some(&token), Some(seen)));
        assert!(!engine.shared.stopped(0, Some(&token), None));
        assert!(matches!(
            engine.transcribe_interruptible(pcm(), RunOptions::default(), &token, seen),
            Err(EngineError::Cancelled)
        ));

        // Not stopped: these get as far as finding no model loaded.
        let current = engine.interrupts();
        assert!(matches!(
            engine.transcribe_interruptible(pcm(), RunOptions::default(), &token, current),
            Err(EngineError::Failed(_))
        ));
        assert!(matches!(
            engine.transcribe_scoped(pcm(), RunOptions::default(), &token),
            Err(EngineError::Failed(_))
        ));
        assert!(!token.is_cancelled());
        assert_eq!(engine.shared.epoch(), 0);
    }

    #[test]
    fn a_cpu_spec_never_checks_gpu_memory() {
        let spec = LoadSpec {
            path: PathBuf::from("model.gguf"),
            backend: Backend::Auto,
            device: DeviceSelector::Key("gpu".to_string()),
            min_gpu_free_bytes: Some(1 << 30),
        };
        let cpu = spec.cpu();
        assert_eq!(cpu.backend, Backend::Cpu);
        assert_eq!(cpu.device, DeviceSelector::Auto);
        assert_eq!(cpu.min_gpu_free_bytes, None);
        assert_eq!(cpu.path, spec.path);
    }

    fn device(key: &str, device_type: DeviceType, memory_free: u64) -> DeviceInfo {
        DeviceInfo {
            name: key.to_string(),
            description: String::new(),
            kind: "vulkan".to_string(),
            device_type,
            device_id: None,
            memory_total: memory_free * 2,
            memory_free,
            index: Some(0),
            key: key.to_string(),
        }
    }

    #[test]
    fn gpu_fits_judges_the_gpu_the_model_would_load_on() {
        const GIB: u64 = 1 << 30;
        let devices = [
            device("cpu", DeviceType::Cpu, 0),
            device("igpu", DeviceType::Igpu, 8 * GIB),
            device("small", DeviceType::Gpu, GIB),
            device("big", DeviceType::Gpu, 16 * GIB),
        ];
        let key = |key: &str| DeviceSelector::Key(key.to_string());

        // A key selects that exact device.
        assert!(gpu_fits(&devices, &key("big"), 4 * GIB));
        assert!(!gpu_fits(&devices, &key("small"), 4 * GIB));
        assert!(gpu_fits(&devices, &key("igpu"), 4 * GIB));
        // An unknown key (or a non-GPU one) falls back to the automatic rule.
        assert!(!gpu_fits(&devices, &key("gone"), 4 * GIB));
        assert!(!gpu_fits(&devices, &key("cpu"), 4 * GIB));
        // Auto prefers the first discrete GPU over an earlier integrated one.
        assert!(!gpu_fits(&devices, &DeviceSelector::Auto, 4 * GIB));
        assert!(gpu_fits(&devices, &DeviceSelector::Auto, GIB));
        let integrated_only = [devices[0].clone(), devices[1].clone()];
        assert!(gpu_fits(&integrated_only, &DeviceSelector::Auto, 4 * GIB));
        assert!(!gpu_fits(&integrated_only, &DeviceSelector::Auto, 9 * GIB));
        // Free memory not reported: nothing to judge.
        let unreported = [device("gpu", DeviceType::Gpu, 0)];
        assert!(gpu_fits(&unreported, &DeviceSelector::Auto, u64::MAX));
        // No GPU at all.
        assert!(gpu_fits(&devices[..1], &DeviceSelector::Auto, u64::MAX));
        assert!(gpu_fits(&[], &key("big"), u64::MAX));
        // An explicit index is never second-guessed.
        assert!(gpu_fits(&devices, &DeviceSelector::Index(3), u64::MAX));
    }

    /// The room a load keeps for another model counts on that load only. A
    /// worker started later for the same spec (after an interrupt) finds
    /// the other model loaded and counted in the free memory, so it needs
    /// the spec's own memory: counting the reserve again would push a model
    /// that fits onto the CPU.
    #[test]
    fn a_load_reserve_counts_only_for_the_load_itself() {
        const GIB: u64 = 1 << 30;
        let spec = LoadSpec {
            path: PathBuf::from("model.gguf"),
            backend: Backend::Auto,
            device: DeviceSelector::Auto,
            min_gpu_free_bytes: Some(5 * GIB),
        };
        let gpu = |free: u64| {
            [
                device("cpu", DeviceType::Cpu, 0),
                device("gpu", DeviceType::Gpu, free),
            ]
        };

        // The load, with the other model still to come: 5 + 1 GiB of 7.
        let at_load = gpu_need(&spec, GIB, false).unwrap();
        assert_eq!(at_load, 6 * GIB);
        assert!(gpu_fits(&gpu(7 * GIB), &spec.device, at_load));
        // A new worker once the other model took 1.5 GiB of the GPU.
        let at_respawn = gpu_need(&spec, 0, false).unwrap();
        assert_eq!(at_respawn, 5 * GIB);
        let after = gpu(11 * GIB / 2);
        assert!(gpu_fits(&after, &spec.device, at_respawn));
        // What keeping the reserve would decide: the CPU.
        assert!(!gpu_fits(&after, &spec.device, at_load));

        // Nothing to check: a CPU-only worker, a pinned device, no need.
        assert_eq!(gpu_need(&spec, GIB, true), None);
        let pinned = LoadSpec {
            device: DeviceSelector::Index(1),
            ..spec.clone()
        };
        assert_eq!(gpu_need(&pinned, GIB, false), None);
        let unchecked = LoadSpec {
            min_gpu_free_bytes: None,
            ..spec.clone()
        };
        assert_eq!(gpu_need(&unchecked, GIB, false), None);
        assert_eq!(gpu_need(&spec, u64::MAX, false), Some(u64::MAX));
    }
}

/// End-to-end checks against a real worker and model. Crash and hang tests
/// need a debug worker exe (fault injection is compiled out of release), and
/// the stream tests a model that can stream.
/// Run with:
/// `HANDY_TRANSCRIBE_WORKER_EXE=target/debug/handy HANDY_TEST_MODEL=<gguf>
///  HANDY_TEST_WAV=<16 kHz mono wav> cargo test --lib engine_supervisor --
///  --ignored --nocapture --test-threads=1`
#[cfg(test)]
mod tests {
    use super::*;
    use transcribe_cpp::TimestampKind;

    fn env_path(name: &str) -> PathBuf {
        PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")))
    }

    fn test_pcm() -> Vec<f32> {
        crate::audio_toolkit::read_wav_samples(env_path("HANDY_TEST_WAV")).expect("read wav")
    }

    fn spec() -> LoadSpec {
        LoadSpec {
            path: env_path("HANDY_TEST_MODEL"),
            backend: Backend::Auto,
            device: DeviceSelector::Auto,
            min_gpu_free_bytes: None,
        }
    }

    /// Faults are read by each worker at spawn, so set them before loading.
    fn set_fault(fault: Option<&str>, gpu_only: bool) {
        let marker = std::env::temp_dir().join("handy-worker-fault-once");
        let _ = std::fs::remove_file(&marker);
        std::env::remove_var("HANDY_WORKER_FAULT_ONCE");
        std::env::remove_var("HANDY_WORKER_FAULT_GPU_ONLY");
        match fault {
            Some(fault) => {
                std::env::set_var("HANDY_WORKER_FAULT", fault);
                if gpu_only {
                    std::env::set_var("HANDY_WORKER_FAULT_GPU_ONLY", "1");
                } else {
                    std::env::set_var("HANDY_WORKER_FAULT_ONCE", marker);
                }
            }
            None => std::env::remove_var("HANDY_WORKER_FAULT"),
        }
    }

    /// The workers alive right now.
    fn worker_count() -> usize {
        LIVE_WORKERS.load(Ordering::SeqCst)
    }

    /// Feed 30 ms frames a little faster than a microphone would.
    fn feed_paced(stream: &StreamHandle, pcm: &[f32]) {
        for frame in pcm.chunks(480) {
            stream.feed(frame.to_vec());
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn stream_all(
        engine: &EngineSupervisor,
        pcm: &[f32],
    ) -> Result<Option<Finalized>, EngineError> {
        let stream =
            engine.start_stream(RunOptions::default(), StreamOptions::default(), |_| {})?;
        feed_paced(&stream, pcm);
        stream.finalize(false)
    }

    /// Block until the owner is waiting on `what` (a [`request_name`]) in
    /// the worker, so a cancel lands on that work rather than before or
    /// after it, however fast the model is.
    fn wait_in_flight(engine: &EngineSupervisor, what: &str) {
        let started = Instant::now();
        while !lock(&engine.shared.in_flight)
            .as_ref()
            .is_some_and(|call| call.what == what)
        {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "{what} never started"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// The test clip repeated to at least `secs` seconds.
    fn long_pcm(secs: usize) -> Vec<f32> {
        let clip = test_pcm();
        clip.repeat((secs * 16_000).div_ceil(clip.len()))
    }

    /// Never two workers: a reload or unload has fully stopped the old
    /// worker before anything else happens.
    #[test]
    #[ignore]
    fn one_worker_at_a_time() {
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        for _ in 0..3 {
            engine.load(spec()).unwrap();
            assert_eq!(worker_count(), 1);
        }
        engine.unload().wait();
        assert_eq!(worker_count(), 0);
        assert!(engine.loaded().is_none());
        // A device probe with nothing loaded leaves no worker behind.
        assert!(engine.devices().is_some());
        assert_eq!(worker_count(), 0);
    }

    /// A worker crash mid-stream loses the stream; the batch fallback then
    /// runs in a fresh worker.
    #[test]
    #[ignore]
    fn crash_mid_stream_falls_back_to_batch() {
        let pcm = test_pcm();
        set_fault(Some("abort@feed"), false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        assert!(stream_all(&engine, &pcm).unwrap().is_none());
        let t = Instant::now();
        let transcript = engine.transcribe(pcm, RunOptions::default()).unwrap();
        println!(
            "batch fallback after crash {:?} on '{}': {}",
            t.elapsed(),
            engine.loaded().unwrap().backend,
            transcript.text
        );
        assert!(engine.loaded().is_some());
        set_fault(None, false);
    }

    /// A hung finalize is killed at its deadline (sized by the buffered
    /// audio, at least the device's call floor) and the batch fallback runs in a fresh
    /// worker.
    #[test]
    #[ignore]
    fn hung_finalize_is_killed_and_recovers() {
        let pcm = test_pcm();
        set_fault(Some("hang@finalize"), false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let t = Instant::now();
        assert!(stream_all(&engine, &pcm).unwrap().is_none());
        println!("stream + hung finalize gave up after {:?}", t.elapsed());
        let t = Instant::now();
        let transcript = engine.transcribe(pcm, RunOptions::default()).unwrap();
        println!(
            "batch fallback after hang {:?}: {}",
            t.elapsed(),
            transcript.text
        );
        set_fault(None, false);
    }

    /// Cancelling a stream finalize reports `Cancelled`, never "no result"
    /// (which would start a batch run), and the next use works.
    #[test]
    #[ignore]
    fn cancel_during_finalize_is_cancelled() {
        let pcm = test_pcm();
        set_fault(Some("hang@finalize"), false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let stream = engine
            .start_stream(RunOptions::default(), StreamOptions::default(), |_| {})
            .unwrap();
        feed_paced(&stream, &pcm);
        let finalize = thread::spawn(move || stream.finalize(false));
        wait_in_flight(&engine, "stream finalize");
        let t = Instant::now();
        engine.cancel();
        let result = finalize.join().unwrap();
        println!("finalize cancelled {:?} after cancel()", t.elapsed());
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(!engine.shared.gpu_unavailable());
        engine.transcribe(pcm, RunOptions::default()).unwrap();
        set_fault(None, false);
    }

    /// Cancelling during a hung feed stops it at once (not at the feed
    /// deadline), skips the queued frames, and the finalize reports
    /// `Cancelled` even though the stream's worker is gone.
    #[test]
    #[ignore]
    fn cancel_during_hung_feed_is_cancelled() {
        let pcm = test_pcm();
        set_fault(Some("hang@feed"), false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let stream = engine
            .start_stream(RunOptions::default(), StreamOptions::default(), |_| {})
            .unwrap();
        feed_paced(&stream, &pcm);
        wait_in_flight(&engine, "stream feed");
        let t = Instant::now();
        engine.cancel();
        let result = stream.finalize(false);
        println!("hung feed cancelled {:?} after cancel()", t.elapsed());
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(!engine.shared.gpu_unavailable());
        engine.transcribe(pcm, RunOptions::default()).unwrap();
        set_fault(None, false);
    }

    /// An unload reads as unloaded at once, even while it waits behind a
    /// run, so a new dictation queues a fresh load instead of assuming the
    /// model is there.
    #[test]
    #[ignore]
    fn unload_reads_unloaded_at_once() {
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let run = {
            let engine = engine.clone();
            thread::spawn(move || engine.transcribe(long_pcm(60), RunOptions::default()))
        };
        wait_in_flight(&engine, "transcription");
        let unload = {
            let engine = engine.clone();
            thread::spawn(move || engine.unload().wait())
        };
        let t = Instant::now();
        while engine.loaded().is_some() {
            assert!(t.elapsed() < Duration::from_secs(1), "still reads loaded");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(lock(&engine.shared.in_flight).is_some(), "run still going");
        run.join().unwrap().unwrap();
        unload.join().unwrap();
        assert!(engine.loaded().is_none());
        assert_eq!(worker_count(), 0);
    }

    /// An unload that is still waiting its turn can't affect a load
    /// requested after it (a new recording right after an "Immediately"
    /// unload): the newer model ends up loaded and usable.
    #[test]
    #[ignore]
    fn queued_unload_never_affects_a_later_load() {
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let run = {
            let engine = engine.clone();
            thread::spawn(move || engine.transcribe(long_pcm(60), RunOptions::default()))
        };
        wait_in_flight(&engine, "transcription");
        drop(engine.unload());
        assert!(
            engine.loaded().is_none(),
            "a new recording must see no model"
        );
        engine.load(spec()).unwrap();
        run.join().unwrap().unwrap();
        assert!(engine.loaded().is_some());
        assert_eq!(worker_count(), 1);
        engine
            .transcribe(test_pcm(), RunOptions::default())
            .unwrap();
    }

    /// A crash that only happens on the GPU: the dictation succeeds on CPU,
    /// the GPU is marked unavailable, and later loads use CPU.
    #[test]
    #[ignore]
    fn gpu_only_crash_recovers_on_cpu() {
        let pcm = test_pcm();
        set_fault(Some("abort@run"), true);
        let engine = EngineSupervisor::new(true);
        let info = engine.load(spec()).unwrap();
        assert!(info.on_gpu, "needs a GPU to test GPU-only faults");
        let transcript = engine
            .transcribe(pcm.clone(), RunOptions::default())
            .unwrap();
        println!("recovered on CPU: {}", transcript.text);
        assert!(engine.shared.gpu_unavailable());
        assert!(!engine.loaded().unwrap().on_gpu);
        let info = engine.load(spec()).unwrap();
        assert!(
            !info.on_gpu,
            "loads stay on CPU while the GPU is marked unavailable"
        );
        engine.transcribe(pcm, RunOptions::default()).unwrap();
        set_fault(None, false);
    }

    /// A GPU driver that crashes while its backend registers, before the
    /// hello: only a CPU-only worker survives it, so that is where the model
    /// loads. The GPU is marked unavailable, later loads stay CPU-only and devices
    /// aren't probed, until "try the GPU again" (a settings change).
    #[test]
    #[ignore]
    fn gpu_backend_init_crash_uses_cpu_only_worker() {
        let pcm = test_pcm();
        set_fault(Some("abort@init"), true);
        let engine = EngineSupervisor::new(true);
        let info = engine.load(spec()).unwrap();
        assert!(!info.on_gpu, "loaded on '{}'", info.backend);
        assert!(engine.shared.gpu_unavailable());
        assert!(engine.devices().is_none());
        engine.transcribe(pcm, RunOptions::default()).unwrap();
        // The fault still fires in any worker with GPU backends.
        assert!(!engine.load(spec()).unwrap().on_gpu);
        set_fault(None, false);
        engine.retry_gpu("test");
        assert!(engine.load(spec()).unwrap().on_gpu);
        assert!(engine.devices().is_some());
    }

    /// With the CPU accelerator, the model worker is CPU-only (a GPU-only
    /// init fault never fires), and a device probe runs beside it rather
    /// than stopping it.
    #[test]
    #[ignore]
    fn cpu_setting_uses_cpu_only_worker_and_probe_leaves_it_running() {
        let pcm = test_pcm();
        set_fault(Some("abort@init"), true);
        let engine = EngineSupervisor::new(true);
        let info = engine
            .load(LoadSpec {
                backend: Backend::Cpu,
                ..spec()
            })
            .unwrap();
        assert!(!info.on_gpu);
        assert!(!engine.shared.gpu_unavailable());
        set_fault(None, false);
        assert!(engine.devices().is_some());
        assert_eq!(worker_count(), 1);
        assert!(engine.loaded().is_some());
        engine.transcribe(pcm, RunOptions::default()).unwrap();
    }

    /// A host that must not use the GPU: even an `Auto` load and the device
    /// probe run CPU-only, so a GPU-only init fault never fires.
    #[test]
    #[ignore]
    fn gpu_disallowed_host_never_registers_a_gpu_backend() {
        set_fault(Some("abort@init"), true);
        let engine = EngineSupervisor::new(false);
        let devices = engine.devices().expect("devices not listed");
        assert!(devices.iter().all(|d| !d.is_gpu()), "{devices:?}");
        assert!(!engine.load(spec()).unwrap().on_gpu);
        assert!(!engine.shared.gpu_unavailable());
        set_fault(None, false);
    }

    /// The mask itself: a CPU-only worker registers no GPU device, including
    /// compiled-in ones such as Metal.
    #[test]
    #[ignore]
    fn cpu_only_worker_lists_no_gpu() {
        set_fault(None, false);
        let mut worker = Worker::spawn(true, false).unwrap();
        let Ok(Response::Hello {
            devices: Some(devices),
        }) = worker.call(&Request::Hello { list_devices: true }, None, HELLO_TIMEOUT)
        else {
            panic!("no device list");
        };
        println!("CPU-only worker devices: {devices:?}");
        assert!(!devices.is_empty());
        assert!(devices.iter().all(|d| !d.is_gpu()));
    }

    /// An explicit device never falls back to CPU.
    #[test]
    #[ignore]
    fn pinned_device_fails_loudly() {
        let pcm = test_pcm();
        set_fault(Some("abort@run"), true);
        let engine = EngineSupervisor::new(true);
        let devices = engine.devices().expect("devices not listed");
        let gpu = devices.iter().find(|d| d.is_gpu()).expect("needs a GPU");
        let info = engine
            .load(LoadSpec {
                device: DeviceSelector::Index(gpu.index.unwrap()),
                ..spec()
            })
            .unwrap();
        assert!(info.on_gpu);
        assert!(engine.transcribe(pcm, RunOptions::default()).is_err());
        assert!(!engine.shared.gpu_unavailable());
        set_fault(None, false);
    }

    /// Cancel mid-run: the run stops promptly, without a retry, the model
    /// still reads as loaded, and the next run works in a fresh worker.
    #[test]
    #[ignore]
    fn cancel_stops_a_run() {
        let pcm = long_pcm(60);
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let run = {
            let engine = engine.clone();
            let pcm = pcm.clone();
            thread::spawn(move || engine.transcribe(pcm, RunOptions::default()))
        };
        wait_in_flight(&engine, "transcription");
        let t = Instant::now();
        engine.cancel();
        let result = run.join().unwrap();
        println!("cancelled {:?} after cancel()", t.elapsed());
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(engine.loaded().is_some());
        assert!(!engine.shared.gpu_unavailable());
        let t = Instant::now();
        engine
            .transcribe(test_pcm(), RunOptions::default())
            .unwrap();
        println!("next run took {:?}", t.elapsed());
    }

    /// Cancel a hung run: killed at once, no retry, no GPU
    /// flag, and the next run starts a fresh worker.
    #[test]
    #[ignore]
    fn cancel_kills_a_hung_run() {
        let pcm = test_pcm();
        set_fault(Some("hang@run"), false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let run = {
            let engine = engine.clone();
            let pcm = pcm.clone();
            thread::spawn(move || engine.transcribe(pcm, RunOptions::default()))
        };
        wait_in_flight(&engine, "transcription");
        // Cancel only once the worker is actually hung, not on its way there.
        let marker = std::env::temp_dir().join("handy-worker-fault-once");
        while !marker.exists() {
            thread::sleep(Duration::from_millis(1));
        }
        let t = Instant::now();
        engine.cancel();
        let result = run.join().unwrap();
        println!("hung run cancelled {:?} after cancel()", t.elapsed());
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(!engine.shared.gpu_unavailable());
        engine.transcribe(pcm, RunOptions::default()).unwrap();
        set_fault(None, false);
    }

    /// A scoped cancel stops its own run at once, leaves the global epoch
    /// alone, and the next (unscoped) run works in a fresh worker.
    #[test]
    #[ignore]
    fn cancel_work_stops_a_scoped_run() {
        let pcm = long_pcm(60);
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let token = WorkToken::new();
        let run = {
            let engine = engine.clone();
            let token = token.clone();
            thread::spawn(move || engine.transcribe_scoped(pcm, RunOptions::default(), &token))
        };
        wait_in_flight(&engine, "transcription");
        let epoch = engine.shared.epoch();
        let t = Instant::now();
        engine.cancel_work(&token);
        let result = run.join().unwrap();
        println!("scoped run cancelled {:?} after cancel_work()", t.elapsed());
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(engine.shared.epoch(), epoch);
        assert!(engine.loaded().is_some());
        assert!(!engine.shared.gpu_unavailable());
        // Work queued with a cancelled token never starts.
        assert!(matches!(
            engine.transcribe_scoped(test_pcm(), RunOptions::default(), &token),
            Err(EngineError::Cancelled)
        ));
        engine
            .transcribe(test_pcm(), RunOptions::default())
            .unwrap();
    }

    /// A scoped cancel leaves an unscoped (dictation) run alone.
    #[test]
    #[ignore]
    fn cancel_work_leaves_an_unscoped_run_alone() {
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let run = {
            let engine = engine.clone();
            thread::spawn(move || engine.transcribe(long_pcm(30), RunOptions::default()))
        };
        wait_in_flight(&engine, "transcription");
        engine.cancel_work(&WorkToken::new());
        run.join().unwrap().unwrap();
    }

    /// File runs ask for segment timestamps (where the model has any) and
    /// get timed rows inside the audio.
    #[test]
    #[ignore]
    fn scoped_run_returns_segment_timestamps() {
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        let info = engine.load(spec()).unwrap();
        let max = info.capabilities.max_timestamp_kind;
        if matches!(max, TimestampKind::None | TimestampKind::Auto) {
            println!("model has no timestamps ({max:?}); nothing to check");
            return;
        }
        let pcm = test_pcm();
        let audio_ms = (pcm.len() / 16) as i64;
        let run = RunOptions {
            timestamps: TimestampKind::Segment,
            ..RunOptions::default()
        };
        let transcript = engine
            .transcribe_scoped(pcm, run, &WorkToken::new())
            .unwrap();
        println!(
            "{:?}: {:?}",
            transcript.timestamp_kind,
            transcript
                .segments
                .iter()
                .map(|s| (s.t0_ms, s.t1_ms, s.text.as_str()))
                .collect::<Vec<_>>()
        );
        assert_eq!(transcript.timestamp_kind, TimestampKind::Segment);
        assert!(transcript
            .segments
            .iter()
            .all(|s| 0 <= s.t0_ms && s.t0_ms <= s.t1_ms && s.t1_ms <= audio_ms + 1_000));
    }

    /// A global cancel (Escape) also stops a scoped run; its token stays
    /// unset, so the file job can tell it apart and retry the chunk.
    #[test]
    #[ignore]
    fn global_cancel_stops_a_scoped_run() {
        let pcm = long_pcm(60);
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        engine.load(spec()).unwrap();
        let token = WorkToken::new();
        let run = {
            let engine = engine.clone();
            let token = token.clone();
            thread::spawn(move || engine.transcribe_scoped(pcm, RunOptions::default(), &token))
        };
        wait_in_flight(&engine, "transcription");
        engine.cancel();
        assert!(matches!(run.join().unwrap(), Err(EngineError::Cancelled)));
        assert!(!token.is_cancelled());
        engine
            .transcribe_scoped(test_pcm(), RunOptions::default(), &token)
            .unwrap();
    }

    /// The recordings engine beside dictation's: two supervisors, one at low
    /// priority, each hold a model and transcribe at the same time.
    #[test]
    #[ignore]
    fn two_supervisors_transcribe_side_by_side() {
        set_fault(None, false);
        let dictation = EngineSupervisor::new(true);
        let recordings = EngineSupervisor::new_with(true, WorkerOptions { low_priority: true });
        dictation.load(spec()).unwrap();
        recordings.load(spec()).unwrap();
        assert_eq!(worker_count(), 2);
        let file = {
            let recordings = recordings.clone();
            thread::spawn(move || {
                recordings.transcribe_scoped(long_pcm(30), RunOptions::default(), &WorkToken::new())
            })
        };
        let t = Instant::now();
        let spoken = dictation
            .transcribe(test_pcm(), RunOptions::default())
            .unwrap();
        println!(
            "dictation beside a file run took {:?}: {}",
            t.elapsed(),
            spoken.text
        );
        file.join().unwrap().unwrap();
        recordings.unload().wait();
        assert_eq!(worker_count(), 1);
        assert!(dictation.loaded().is_some());
    }

    /// An interrupt stops a scoped run like a cancel, but leaves its token
    /// and the epoch alone, and isn't held against the GPU: the next run
    /// starts a fresh worker on the same device.
    #[test]
    #[ignore]
    fn interrupt_scoped_work_stops_a_run_that_can_be_redone() {
        set_fault(None, false);
        let engine = EngineSupervisor::new_with(true, WorkerOptions { low_priority: true });
        let on_gpu = engine.load(spec()).unwrap().on_gpu;
        let token = WorkToken::new();
        let run = {
            let engine = engine.clone();
            let token = token.clone();
            thread::spawn(move || {
                engine.transcribe_scoped(long_pcm(60), RunOptions::default(), &token)
            })
        };
        wait_in_flight(&engine, "transcription");
        let epoch = engine.shared.epoch();
        let t = Instant::now();
        assert!(engine.interrupt_scoped_work());
        let result = run.join().unwrap();
        println!("scoped run interrupted {:?} after the call", t.elapsed());
        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(!token.is_cancelled());
        assert_eq!(engine.shared.epoch(), epoch);
        assert!(!engine.shared.gpu_unavailable());
        let t = Instant::now();
        engine
            .transcribe_scoped(test_pcm(), RunOptions::default(), &token)
            .unwrap();
        println!("redone after a respawn in {:?}", t.elapsed());
        assert_eq!(engine.loaded().unwrap().on_gpu, on_gpu);
        // Nothing scoped in flight now.
        assert!(!engine.interrupt_scoped_work());
    }

    /// A GPU crash in one supervisor's worker marks only that supervisor's
    /// GPU unavailable: the other keeps its model on the GPU.
    #[test]
    #[ignore]
    fn a_gpu_crash_in_one_supervisor_leaves_the_other_on_the_gpu() {
        set_fault(None, false);
        let dictation = EngineSupervisor::new(true);
        if !dictation.load(spec()).unwrap().on_gpu {
            println!("the model did not load on a GPU; nothing to check");
            return;
        }
        // Faults are read at spawn, so only the second supervisor's worker
        // has it.
        set_fault(Some("abort@run"), true);
        let recordings = EngineSupervisor::new_with(true, WorkerOptions { low_priority: true });
        assert!(recordings.load(spec()).unwrap().on_gpu);
        recordings
            .transcribe(test_pcm(), RunOptions::default())
            .unwrap();
        assert!(recordings.shared.gpu_unavailable());
        assert!(!recordings.loaded().unwrap().on_gpu);
        set_fault(None, false);
        assert!(!dictation.shared.gpu_unavailable());
        dictation
            .transcribe(test_pcm(), RunOptions::default())
            .unwrap();
        assert!(dictation.loaded().unwrap().on_gpu);
    }

    /// A GPU that reports too little free memory gets the model loaded on
    /// the CPU instead, marked as such, also after a respawn.
    #[test]
    #[ignore]
    fn too_little_gpu_memory_loads_on_the_cpu() {
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        let devices = engine.devices().expect("devices not listed");
        if gpu_fits(&devices, &DeviceSelector::Auto, u64::MAX) {
            println!("no GPU reports its free memory; nothing to check");
            return;
        }
        let info = engine
            .load(LoadSpec {
                min_gpu_free_bytes: Some(u64::MAX),
                ..spec()
            })
            .unwrap();
        assert!(!info.on_gpu, "loaded on '{}'", info.backend);
        assert!(info.gpu_memory_fallback);
        assert!(!engine.shared.gpu_unavailable());
        let token = WorkToken::new();
        let run = {
            let engine = engine.clone();
            let token = token.clone();
            thread::spawn(move || {
                engine.transcribe_scoped(long_pcm(30), RunOptions::default(), &token)
            })
        };
        wait_in_flight(&engine, "transcription");
        assert!(engine.interrupt_scoped_work());
        assert!(matches!(run.join().unwrap(), Err(EngineError::Cancelled)));
        engine
            .transcribe_scoped(test_pcm(), RunOptions::default(), &token)
            .unwrap();
        let info = engine.loaded().unwrap();
        assert!(!info.on_gpu);
        assert!(info.gpu_memory_fallback);
        // Without the requirement it goes to the GPU.
        assert!(engine.load(spec()).unwrap().on_gpu);
    }

    /// Room a load reserves for another model is checked on that load
    /// only: a worker started after an interrupt checks the spec's own
    /// need, so the model goes to the GPU then.
    #[test]
    #[ignore]
    fn a_load_reserve_is_not_kept_for_a_new_worker() {
        set_fault(None, false);
        let engine = EngineSupervisor::new(true);
        let devices = engine.devices().expect("devices not listed");
        if gpu_fits(&devices, &DeviceSelector::Auto, u64::MAX) {
            println!("no GPU reports its free memory; nothing to check");
            return;
        }
        let checked = LoadSpec {
            min_gpu_free_bytes: Some(0),
            ..spec()
        };
        let info = engine.load_reserving(checked, u64::MAX).unwrap();
        assert!(!info.on_gpu, "loaded on '{}'", info.backend);
        assert!(info.gpu_memory_fallback);
        let token = WorkToken::new();
        let run = {
            let engine = engine.clone();
            let token = token.clone();
            thread::spawn(move || {
                engine.transcribe_scoped(long_pcm(30), RunOptions::default(), &token)
            })
        };
        wait_in_flight(&engine, "transcription");
        assert!(engine.interrupt_scoped_work());
        assert!(matches!(run.join().unwrap(), Err(EngineError::Cancelled)));
        engine
            .transcribe_scoped(test_pcm(), RunOptions::default(), &token)
            .unwrap();
        let info = engine.loaded().unwrap();
        assert!(info.on_gpu, "redone on '{}'", info.backend);
        assert!(!info.gpu_memory_fallback);
    }

    /// A worker hung in native code still exits when its parent goes away
    /// (stdin closes), without being killed.
    #[test]
    #[ignore]
    fn hung_worker_exits_when_stdin_closes() {
        set_fault(Some("hang@run"), false);
        let mut worker = Worker::spawn(false, false).unwrap();
        worker
            .call(
                &Request::Hello {
                    list_devices: false,
                },
                None,
                HELLO_TIMEOUT,
            )
            .unwrap();
        let spec = spec();
        let load = Request::Load {
            path: spec.path,
            backend: spec.backend,
            device: spec.device,
        };
        worker.call(&load, None, LOAD_TIMEOUT).unwrap();
        let run = Request::Run {
            options: RunOptions::default(),
        };
        assert!(worker
            .control
            .send(encode_message(&run, Some(&test_pcm())).unwrap()));
        thread::sleep(Duration::from_secs(1));
        worker.control.close_stdin();
        let t = Instant::now();
        let status = loop {
            if let Some(status) = lock(&worker.control.child).try_wait().unwrap() {
                break status;
            }
            assert!(t.elapsed() < SHUTDOWN_GRACE, "worker outlived its stdin");
            thread::sleep(Duration::from_millis(10));
        };
        println!(
            "hung worker exited {:?} after stdin closed: {status}",
            t.elapsed()
        );
        assert!(status.success());
        set_fault(None, false);
    }
}
