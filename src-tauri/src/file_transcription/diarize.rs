//! Speaker diarization of a file with NVIDIA Sortformer, in a dedicated
//! CPU-only, low-priority engine worker that never touches dictation's engine.
//!
//! Sortformer runs over all the audio it is given at once, and its memory
//! grows with that audio (about 75 MB per minute). Long files are therefore
//! diarized in windows of 20 minutes that overlap by 90 seconds: the file is
//! decoded once into a buffer that never holds more than one window, each
//! window runs in turn on the same worker, and the speaker ids of a window
//! are linked to those of the window before it by who speaks when in their
//! overlap. A file of up to 21.5 minutes is a single window.
//!
//! A speaker who says little or nothing in an overlap cannot be linked
//! there. So a few seconds of each speaker's clean speech (an exemplar) are
//! kept as windows are diarized, and a later window is diarized with the
//! exemplars of the speakers it may not otherwise recognise placed before
//! it: whoever the diarizer hears in an exemplar is that speaker.

use crate::engine_supervisor::{
    DeviceSelector, EngineError, EngineSupervisor, LoadSpec, WorkToken, WorkerOptions,
};
use crate::file_transcription::decode::{decode_to_16k_mono, DecodeError, DecodeOptions};
use crate::file_transcription::speakers::{normalize_turns, SpeakerTurn};
use crate::file_transcription::types::{SpeakerJobState, SpeakerSkipReason};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use transcribe_cpp::{
    Backend, Diarize, RunExtension, RunOptions, SortformerPreset, SortformerStreamOptions,
    SpeakerSegment, TimestampKind,
};

/// Diarizer id stored in `TranscriptSpeakers.model`.
pub const DIARIZER_MODEL_ID: &str = "sortformer-4spk-v2.1-q8_0";
/// Sanity bound on the audio length to diarize. Long files are diarized in
/// windows, so this is not a memory limit.
pub const MAX_DIARIZE_MS: u64 = 12 * 3_600_000;
/// How often a held diarization re-checks dictation, and `wait` its token.
pub const DIARIZE_POLL: Duration = Duration::from_millis(150);

/// Audio per diarizer run. The worker's peak memory follows this (about
/// 1.6 GB for 21.5 minutes), not the file's length.
const WINDOW_MS: u64 = 20 * 60_000;
/// How much consecutive windows share, to link their speaker ids.
const OVERLAP_MS: u64 = 90_000;
/// A window speaker is linked to an earlier one only when the two talk
/// together for at least this long in the overlap...
const LINK_MIN_MS: u64 = 3_000;
/// ...and for at least this share (in percent) of the quieter one's speech
/// there.
const LINK_MIN_SHARE_PERCENT: u64 = 50;
/// Speakers the model tells apart in one run.
const MODEL_SPEAKERS: usize = 4;
/// A speaker who talks this long in an overlap is linked there and needs no
/// exemplar.
const EXEMPLAR_CLEAR_MS: u64 = 10_000;
/// An exemplar is cut from a speaker's longest clean turns until it is this
/// long...
const EXEMPLAR_TARGET_MS: u64 = 8_000;
/// ...but no longer than this...
const EXEMPLAR_MAX_MS: u64 = 10_000;
/// ...and a speaker with less clean speech than this gets none.
const EXEMPLAR_MIN_MS: u64 = 3_000;
/// Pieces shorter than this are not worth cutting.
const EXEMPLAR_MIN_PIECE_MS: u64 = 1_000;
/// Cut away from each end of a turn, where speakers hand over.
const EXEMPLAR_EDGE_MS: u64 = 200;
/// Silence after each exemplar, so they never run into each other or into
/// the window.
const EXEMPLAR_GAP_MS: u64 = 1_000;
/// A window speaker is linked to an exemplar's speaker when heard over at
/// least this much of it (and half of it, as in the overlap).
const EXEMPLAR_LINK_MIN_MS: u64 = 1_500;
/// Exemplars kept at most (about 640 kB each); the speakers who talked
/// least lose theirs first.
const MAX_EXEMPLARS: usize = 16;
const SAMPLES_PER_MS: u64 = 16;
/// The architecture a speaker model must load as.
const SORTFORMER_ARCH: &str = "sortformer";

#[derive(Clone, Debug)]
pub struct DiarizeRequest {
    pub path: PathBuf,
    pub decode: DecodeOptions,
    pub model_path: PathBuf,
    /// Probe estimate: sizes the sample buffer and fails early with TooLong.
    pub estimated_duration_ms: Option<u64>,
    /// Diarize only the audio before this point (the transcribed span of a
    /// partial transcript); None for the whole file.
    pub limit_ms: Option<u64>,
}

/// How far a diarization has got, reported as it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiarizeProgress {
    /// Held before a window while dictation is active (sent once per window).
    Waiting,
    /// A window is being diarized.
    Running,
    /// The windows up to `through_ms` are done. Never decreases, because
    /// each window ends later than the one before.
    Diarized { through_ms: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiarizeError {
    ModelMissing(PathBuf),
    /// Longer than `MAX_DIARIZE_MS`.
    TooLong {
        duration_ms: u64,
    },
    Decode(String),
    Engine(String),
    Cancelled,
}

impl DiarizeError {
    /// ModelMissing → Skipped{ModelMissing}; Cancelled → Failed{"cancelled"};
    /// others (TooLong included) → Failed{message}.
    pub fn job_state(&self) -> SpeakerJobState {
        match self {
            Self::ModelMissing(_) => SpeakerJobState::Skipped {
                reason: SpeakerSkipReason::ModelMissing,
            },
            Self::Cancelled => SpeakerJobState::Failed {
                message: "cancelled".to_string(),
            },
            other => SpeakerJobState::Failed {
                message: other.to_string(),
            },
        }
    }
}

impl fmt::Display for DiarizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelMissing(path) => {
                write!(f, "the speaker model is missing: {}", path.display())
            }
            Self::TooLong { duration_ms } => write!(
                f,
                "the recording is too long to label speakers ({} min)",
                duration_ms / 60_000
            ),
            Self::Decode(message) => write!(f, "could not decode the audio: {}", message),
            Self::Engine(message) => write!(f, "the speaker model failed: {}", message),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for DiarizeError {}

impl From<EngineError> for DiarizeError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::Cancelled => Self::Cancelled,
            EngineError::Failed(message) => Self::Engine(message),
        }
    }
}

/// Runs Sortformer on 16 kHz mono samples in `engine` (a dedicated
/// supervisor). Loads `model_path` if not loaded, then one
/// `transcribe_scoped` with the offline (very high latency) preset. Returns
/// the raw turns, timed from the start of `samples` (unsorted; normalized
/// later by `speakers::normalize_turns`).
pub fn diarize_samples(
    engine: &EngineSupervisor,
    model_path: &Path,
    samples: Vec<f32>,
    token: &WorkToken,
) -> Result<Vec<SpeakerTurn>, DiarizeError> {
    if engine.loaded().is_none() {
        if !model_path.is_file() {
            return Err(DiarizeError::ModelMissing(model_path.to_path_buf()));
        }
        let info = engine.load(LoadSpec {
            path: model_path.to_path_buf(),
            backend: Backend::Cpu,
            device: DeviceSelector::Auto,
            min_gpu_free_bytes: None,
        })?;
        if info.arch != SORTFORMER_ARCH {
            // Free it now; the next call would load it again and fail the same way.
            engine.unload();
            return Err(DiarizeError::Engine(format!(
                "{} is a {} model, not a Sortformer speaker model",
                model_path.display(),
                info.arch
            )));
        }
    }
    let run = RunOptions {
        // Sortformer always diarizes; On documents the intent.
        diarize: Diarize::On,
        timestamps: TimestampKind::Auto,
        family: Some(RunExtension::Sortformer(SortformerStreamOptions {
            preset: Some(SortformerPreset::VeryHighLatency),
        })),
        ..RunOptions::default()
    };
    let transcript = engine.transcribe_scoped(samples, run, token)?;
    Ok(rows_to_turns(&transcript.speaker_segments))
}

/// Sortformer's rows as turns: rows without a speaker or a duration are
/// dropped. Ids stay raw (1-based, sparse).
fn rows_to_turns(rows: &[SpeakerSegment]) -> Vec<SpeakerTurn> {
    rows.iter()
        .filter(|row| row.speaker_id > 0 && row.t1_ms > row.t0_ms)
        .map(|row| SpeakerTurn {
            start_ms: row.t0_ms.max(0) as u64,
            end_ms: row.t1_ms.max(0) as u64,
            speaker: row.speaker_id as u32,
        })
        .filter(|turn| turn.end_ms > turn.start_ms)
        .collect()
}

/// A diarization running on its own "file-diarize" thread.
pub struct DiarizationHandle {
    /// None once waited for.
    thread: Option<JoinHandle<Result<Vec<SpeakerTurn>, DiarizeError>>>,
    /// Closed when the thread ends (by its result or a panic).
    done: mpsc::Receiver<()>,
    /// The job's token: cancelling the job cancels the diarization.
    job: WorkToken,
    /// This diarization's own token, so [`cancel`](Self::cancel) stops it
    /// without cancelling the job.
    token: WorkToken,
    /// The diarizer's engine while it exists, so a cancel can stop its run.
    engine: Arc<Mutex<Option<EngineSupervisor>>>,
    /// What the thread reports as it goes; see [`Self::wait_reporting`].
    progress: mpsc::Receiver<DiarizeProgress>,
}

/// Starts diarizing `req.path` on a "file-diarize" thread, waiting while
/// `hold()` (dictation active) before each diarizer run. Cancelling `token`
/// (the job's) stops it.
pub fn start_diarization(
    req: DiarizeRequest,
    token: WorkToken,
    hold: Arc<dyn Fn() -> bool + Send + Sync>,
) -> DiarizationHandle {
    let own = WorkToken::new();
    let engine = Arc::new(Mutex::new(None));
    let (done_tx, done) = mpsc::channel::<()>();
    let (progress_tx, progress) = mpsc::channel();
    let context = Diarization {
        job: token.clone(),
        token: own.clone(),
        hold,
        engine: Arc::clone(&engine),
        limits: Limits::default(),
        progress: progress_tx,
    };
    let spawned = thread::Builder::new()
        .name("file-diarize".into())
        .spawn(move || {
            let _done = done_tx;
            context.run(&req)
        });
    let thread = match spawned {
        Ok(thread) => Some(thread),
        Err(e) => {
            warn!("Could not start the speaker labelling thread: {}", e);
            // `done` is already closed; `wait` reports the failure.
            None
        }
    };
    DiarizationHandle {
        thread,
        done,
        job: token,
        token: own,
        engine,
        progress,
    }
}

impl DiarizationHandle {
    /// Blocks until the diarization ends. Polls the job's token every
    /// DIARIZE_POLL and, on cancel, kills only the diarizer worker.
    pub fn wait(self) -> Result<Vec<SpeakerTurn>, DiarizeError> {
        self.wait_reporting(&mut |_| {})
    }

    /// [`Self::wait`], passing what the diarization reports to `on_progress`
    /// on the waiting thread, at most every DIARIZE_POLL.
    pub fn wait_reporting(
        mut self,
        on_progress: &mut dyn FnMut(DiarizeProgress),
    ) -> Result<Vec<SpeakerTurn>, DiarizeError> {
        let Some(thread) = self.thread.take() else {
            return Err(DiarizeError::Engine(
                "the speaker labelling thread could not start".to_string(),
            ));
        };
        let mut cancelled = false;
        while let Err(mpsc::RecvTimeoutError::Timeout) = self.done.recv_timeout(DIARIZE_POLL) {
            self.progress.try_iter().for_each(&mut *on_progress);
            if !cancelled && self.job.is_cancelled() {
                self.cancel();
                cancelled = true;
            }
        }
        // The thread has ended, so everything it reported is here.
        self.progress.try_iter().for_each(&mut *on_progress);
        thread.join().unwrap_or_else(|_| {
            Err(DiarizeError::Engine(
                "the speaker labelling thread stopped unexpectedly".to_string(),
            ))
        })
    }

    /// Cancels the diarization and its worker's run; returns at once. The
    /// job's token is left alone.
    pub fn cancel(&self) {
        self.token.cancel();
        let engine = self
            .engine
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(engine) = engine {
            engine.cancel_work(&self.token);
        }
    }
}

impl Drop for DiarizationHandle {
    /// A handle dropped without `wait` (the job ended early) stops the
    /// diarization rather than leave it running in the background.
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.cancel();
        }
    }
}

/// Sizes that bound a diarization; small in tests.
#[derive(Clone, Copy, Debug)]
struct Limits {
    window_ms: u64,
    overlap_ms: u64,
    max_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            window_ms: WINDOW_MS,
            overlap_ms: OVERLAP_MS,
            max_ms: MAX_DIARIZE_MS,
        }
    }
}

/// The "file-diarize" thread's state.
struct Diarization {
    job: WorkToken,
    token: WorkToken,
    hold: Arc<dyn Fn() -> bool + Send + Sync>,
    engine: Arc<Mutex<Option<EngineSupervisor>>>,
    limits: Limits,
    /// Send errors are ignored: nobody may be listening.
    progress: mpsc::Sender<DiarizeProgress>,
}

/// The decode sink's error once it has all the audio `limit_ms` asks for.
const REACHED_LIMIT: &str = "the speaker labelling limit was reached";

impl Diarization {
    fn stopped(&self) -> bool {
        self.job.is_cancelled() || self.token.is_cancelled()
    }

    fn run(&self, req: &DiarizeRequest) -> Result<Vec<SpeakerTurn>, DiarizeError> {
        let started = Instant::now();
        let result = self.diarize(req);
        // Dropping the last handle on the engine stops its worker, which
        // returns the worker's memory at once.
        self.engine
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match &result {
            Ok(turns) => info!(
                "Labelled speakers of {} in {:.1}s ({} turns)",
                req.path.display(),
                started.elapsed().as_secs_f64(),
                turns.len()
            ),
            Err(e) => info!(
                "Speaker labelling of {} ended after {:.1}s: {}",
                req.path.display(),
                started.elapsed().as_secs_f64(),
                e
            ),
        }
        result
    }

    fn diarize(&self, req: &DiarizeRequest) -> Result<Vec<SpeakerTurn>, DiarizeError> {
        if !req.model_path.is_file() {
            return Err(DiarizeError::ModelMissing(req.model_path.clone()));
        }
        // Only the audio before the limit is diarized.
        let estimated_duration_ms = match (req.estimated_duration_ms, req.limit_ms) {
            (Some(estimate), Some(limit)) => Some(estimate.min(limit)),
            (estimate, _) => estimate,
        };
        if let Some(duration_ms) = estimated_duration_ms.filter(|&ms| ms > self.limits.max_ms) {
            return Err(DiarizeError::TooLong { duration_ms });
        }

        let mut stitcher = Stitcher::default();
        let mut exemplars = Exemplars::default();
        let mut engine: Option<EngineSupervisor> = None;
        let mut run_window =
            |start: u64, samples: Vec<f32>, more: bool| -> Result<(), DiarizeError> {
                let window = Window {
                    start,
                    samples,
                    more,
                };
                self.run_window(req, &mut engine, &mut stitcher, &mut exemplars, window)
            };
        self.decode_windows(req, estimated_duration_ms, &mut run_window)?;
        Ok(stitcher.finish())
    }

    /// Decodes `req.path`, up to `req.limit_ms`, and hands its audio to
    /// `run` window by window.
    fn decode_windows(
        &self,
        req: &DiarizeRequest,
        estimated_duration_ms: Option<u64>,
        run: &mut RunWindow<'_>,
    ) -> Result<(), DiarizeError> {
        let max_samples = self.limits.max_ms * SAMPLES_PER_MS;
        let limit_samples = req.limit_ms.map(|ms| ms * SAMPLES_PER_MS);
        let mut windows = Windows::new(self.limits, estimated_duration_ms);
        let mut failure: Option<DiarizeError> = None;
        let mut decoded: u64 = 0;
        let mut reached_limit = false;
        let decode = decode_to_16k_mono(
            &req.path,
            &req.decode,
            self.token.flag(),
            &mut |block: &[f32]| {
                // The decoder only watches this diarization's own token.
                if self.job.is_cancelled() {
                    self.token.cancel();
                    return Ok(());
                }
                let block = match limit_samples {
                    Some(limit) if decoded + block.len() as u64 >= limit => {
                        reached_limit = true;
                        &block[..(limit.saturating_sub(decoded)) as usize]
                    }
                    _ => block,
                };
                decoded += block.len() as u64;
                let step = if decoded > max_samples {
                    Err(DiarizeError::TooLong {
                        duration_ms: decoded / SAMPLES_PER_MS,
                    })
                } else {
                    windows.push(block, run)
                };
                step.map_err(|e| {
                    let message = anyhow::anyhow!("speaker labelling stopped: {e}");
                    failure = Some(e);
                    message
                })?;
                if reached_limit {
                    // Stops the decoder; the audio after the limit is not needed.
                    return Err(anyhow::anyhow!(REACHED_LIMIT));
                }
                Ok(())
            },
        );
        if let Some(e) = failure {
            return Err(e);
        }
        match decode {
            Ok(outcome) => {
                if let Some(reason) = outcome.truncated {
                    warn!(
                        "Labelling speakers of the decodable part of {}: {}",
                        req.path.display(),
                        reason
                    );
                }
            }
            Err(DecodeError::Cancelled) => return Err(DiarizeError::Cancelled),
            Err(_) if self.stopped() => return Err(DiarizeError::Cancelled),
            Err(_) if reached_limit => debug!(
                "Labelling speakers of {} up to {} s",
                req.path.display(),
                decoded / SAMPLES_PER_MS / 1000
            ),
            Err(e) => return Err(DiarizeError::Decode(e.to_string())),
        }
        if self.stopped() {
            return Err(DiarizeError::Cancelled);
        }
        windows.finish(run)
    }

    /// Diarizes one window once dictation is idle, after the exemplars of
    /// the speakers it may not otherwise recognise, links its speakers, and
    /// keeps exemplars from it for the windows after it.
    fn run_window(
        &self,
        req: &DiarizeRequest,
        engine: &mut Option<EngineSupervisor>,
        stitcher: &mut Stitcher,
        exemplars: &mut Exemplars,
        window: Window,
    ) -> Result<(), DiarizeError> {
        let Window {
            start,
            samples,
            more,
        } = window;
        let mut held = false;
        while (self.hold)() {
            if self.stopped() {
                return Err(DiarizeError::Cancelled);
            }
            if !held {
                debug!("Speaker labelling waits while dictation is active");
                let _ = self.progress.send(DiarizeProgress::Waiting);
                held = true;
            }
            thread::sleep(DIARIZE_POLL);
        }
        if self.stopped() {
            return Err(DiarizeError::Cancelled);
        }
        let engine = engine.get_or_insert_with(|| {
            let engine = EngineSupervisor::new_with(false, WorkerOptions { low_priority: true });
            *self.engine.lock().unwrap_or_else(PoisonError::into_inner) = Some(engine.clone());
            engine
        });

        let start_ms = start / SAMPLES_PER_MS;
        let end_ms = start_ms + samples.len() as u64 / SAMPLES_PER_MS;
        let chosen = exemplars.choose(&stitcher.overlap_speech(start_ms));
        let (mut input, spans) = exemplars.prefix(&chosen);
        let prefix_ms = input.len() as u64 / SAMPLES_PER_MS;
        // The window's own audio is kept only while a later window needs
        // exemplars from it.
        let samples = if input.is_empty() && !more {
            input = samples;
            None
        } else {
            input.extend_from_slice(&samples);
            more.then_some(samples)
        };
        let started = Instant::now();
        let _ = self.progress.send(DiarizeProgress::Running);
        let turns = diarize_samples(engine, &req.model_path, input, &self.token)?;
        debug!(
            "Diarized {}–{} s of {} (after {} exemplar(s)) in {:.1}s: {} turns",
            start_ms / 1000,
            end_ms / 1000,
            req.path.display(),
            spans.len(),
            started.elapsed().as_secs_f64(),
            turns.len()
        );
        let (prefix_turns, turns) = split_prefix(&turns, prefix_ms);
        let turns: Vec<SpeakerTurn> = turns
            .into_iter()
            .map(|turn| SpeakerTurn {
                start_ms: turn.start_ms + start_ms,
                end_ms: turn.end_ms + start_ms,
                ..turn
            })
            .collect();
        let prefix = Prefix {
            spans,
            turns: prefix_turns,
        };
        let ids = stitcher.add(start_ms, end_ms, turns.clone(), &prefix);
        let _ = self
            .progress
            .send(DiarizeProgress::Diarized { through_ms: end_ms });
        if let Some(samples) = samples {
            exemplars.update(&samples, start_ms, &turns, &ids);
        }
        Ok(())
    }
}

/// Cuts the decoded audio into windows of `window` samples that overlap by
/// `overlap`, holding at most `window + overlap` samples (plus one decoded
/// block). A window is run once the audio is known to go on past it by more
/// than the overlap, so the last window takes up to `window + overlap` and is
/// never just a sliver.
struct Windows {
    window: usize,
    overlap: usize,
    buffer: Vec<f32>,
    /// File position of `buffer[0]`, in samples.
    start: u64,
}

/// Runs a window: its start in the file (in samples), its samples, and
/// whether more windows follow.
type RunWindow<'a> = dyn FnMut(u64, Vec<f32>, bool) -> Result<(), DiarizeError> + 'a;

/// One window to diarize, as [`Windows`] hands it over.
struct Window {
    /// Position in the file, in samples.
    start: u64,
    samples: Vec<f32>,
    /// Whether more windows follow.
    more: bool,
}

impl Windows {
    fn new(limits: Limits, estimated_duration_ms: Option<u64>) -> Self {
        let window = (limits.window_ms * SAMPLES_PER_MS) as usize;
        let overlap = (limits.overlap_ms * SAMPLES_PER_MS)
            .min(limits.window_ms * SAMPLES_PER_MS / 2) as usize;
        let capacity = estimated_duration_ms.map_or(window + overlap, |ms| {
            // Room for the estimate being a little short.
            ((ms * SAMPLES_PER_MS) as usize + 16_000).min(window + overlap)
        });
        Self {
            window,
            overlap,
            buffer: Vec::with_capacity(capacity),
            start: 0,
        }
    }

    fn push(&mut self, block: &[f32], run: &mut RunWindow<'_>) -> Result<(), DiarizeError> {
        self.buffer.extend_from_slice(block);
        while self.buffer.len() > self.window + self.overlap {
            let stride = self.window - self.overlap;
            let mut rest = Vec::with_capacity(self.window + self.overlap);
            rest.extend_from_slice(&self.buffer[stride..]);
            let mut window = std::mem::replace(&mut self.buffer, rest);
            window.truncate(self.window);
            run(self.start, window, true)?;
            self.start += stride as u64;
        }
        Ok(())
    }

    /// Runs the last window, if any audio is left.
    fn finish(self, run: &mut RunWindow<'_>) -> Result<(), DiarizeError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        run(self.start, self.buffer, false)
    }
}

/// Joins the turns of consecutive windows into one timeline with file-wide
/// speaker ids. In each overlap, turns come from the earlier window up to
/// its midpoint and from the later one after it, so nothing is counted twice.
#[derive(Default)]
struct Stitcher {
    /// Final turns, up to the last cut.
    done: Vec<SpeakerTurn>,
    /// The last window's turns from its cut on, with file-wide ids.
    pending: Vec<SpeakerTurn>,
    /// End of the last window, in ms.
    pending_end_ms: u64,
    windows: u32,
    /// File-wide ids handed out so far.
    ids: u32,
}

impl Stitcher {
    /// How long each file-wide speaker talks in the overlap of the last
    /// window with the next one, which starts at `start_ms`.
    fn overlap_speech(&self, start_ms: u64) -> HashMap<u32, u64> {
        let mut speech: HashMap<u32, u64> = HashMap::new();
        for turn in clip(&self.pending, start_ms, self.pending_end_ms) {
            *speech.entry(turn.speaker).or_default() += turn.end_ms - turn.start_ms;
        }
        speech
    }

    /// `turns` are in file time with the window's own speaker ids; `prefix`
    /// is what the diarizer heard in the exemplars before the window. Links
    /// the window's speakers by the overlap first, then by the exemplars,
    /// and returns the window's id → file-wide id.
    fn add(
        &mut self,
        start_ms: u64,
        end_ms: u64,
        turns: Vec<SpeakerTurn>,
        prefix: &Prefix,
    ) -> HashMap<u32, u32> {
        let (mut links, cut_ms) = if self.windows == 0 {
            (HashMap::new(), start_ms)
        } else {
            let overlap_end = self.pending_end_ms.max(start_ms);
            (
                link_speakers(&self.pending, &turns, start_ms, overlap_end, LINK_MIN_MS),
                start_ms + (overlap_end - start_ms) / 2,
            )
        };
        let by_exemplar = link_exemplars(prefix, &links);
        if self.windows > 0 {
            debug!(
                "Speaker ids of the window from {} s linked as {:?} by the overlap, \
                 {:?} by exemplars",
                start_ms / 1000,
                links,
                by_exemplar
            );
        }
        links.extend(by_exemplar);

        // New ids for the window's other speakers, in order of first turn.
        let mut ids = links;
        let mut ordered = turns.clone();
        ordered.sort_by_key(|turn| (turn.start_ms, turn.speaker));
        for turn in &ordered {
            if !ids.contains_key(&turn.speaker) {
                self.ids += 1;
                ids.insert(turn.speaker, self.ids);
            }
        }

        let pending = std::mem::take(&mut self.pending);
        self.done.extend(clip(&pending, 0, cut_ms));
        let global: Vec<SpeakerTurn> = turns
            .iter()
            .map(|turn| SpeakerTurn {
                speaker: ids[&turn.speaker],
                ..*turn
            })
            .collect();
        self.pending = clip(&global, cut_ms, u64::MAX);
        self.pending_end_ms = end_ms;
        self.windows += 1;
        ids
    }

    fn finish(mut self) -> Vec<SpeakerTurn> {
        self.done.append(&mut self.pending);
        self.done
    }
}

/// `turns` cut to `from..to`, dropping what falls outside.
fn clip(turns: &[SpeakerTurn], from: u64, to: u64) -> Vec<SpeakerTurn> {
    turns
        .iter()
        .map(|turn| SpeakerTurn {
            start_ms: turn.start_ms.max(from),
            end_ms: turn.end_ms.min(to),
            speaker: turn.speaker,
        })
        .filter(|turn| turn.end_ms > turn.start_ms)
        .collect()
}

/// Links the speakers of a new window to those of the previous one by how
/// long they talk at the same time in the shared span `from..to`: greedily,
/// largest co-occurrence first, accepting a pair only when it covers at
/// least `min_ms` and `LINK_MIN_SHARE_PERCENT` of the quieter one's speech
/// there. Each id is linked at most once. Returns new id → old id.
fn link_speakers(
    previous: &[SpeakerTurn],
    next: &[SpeakerTurn],
    from: u64,
    to: u64,
    min_ms: u64,
) -> HashMap<u32, u32> {
    let previous = clip(previous, from, to);
    let next = clip(next, from, to);
    let speech = |turns: &[SpeakerTurn]| {
        let mut totals: HashMap<u32, u64> = HashMap::new();
        for turn in turns {
            *totals.entry(turn.speaker).or_default() += turn.end_ms - turn.start_ms;
        }
        totals
    };
    let previous_speech = speech(&previous);
    let next_speech = speech(&next);

    let mut together: HashMap<(u32, u32), u64> = HashMap::new();
    for old in &previous {
        for new in &next {
            let shared = old
                .end_ms
                .min(new.end_ms)
                .saturating_sub(old.start_ms.max(new.start_ms));
            if shared > 0 {
                *together.entry((old.speaker, new.speaker)).or_default() += shared;
            }
        }
    }
    let mut pairs: Vec<((u32, u32), u64)> = together.into_iter().collect();
    pairs.sort_by_key(|&((old, new), ms)| (std::cmp::Reverse(ms), old, new));

    let mut links = HashMap::new();
    let mut linked_old = Vec::new();
    for ((old, new), ms) in pairs {
        if links.contains_key(&new) || linked_old.contains(&old) {
            continue;
        }
        let quieter = previous_speech[&old].min(next_speech[&new]);
        if ms >= min_ms && ms * 100 >= quieter * LINK_MIN_SHARE_PERCENT {
            links.insert(new, old);
            linked_old.push(old);
        }
    }
    links
}

/// What came before a window in its diarizer run: the exemplars, and what
/// the diarizer heard in them. Times are from the start of the run.
#[derive(Default)]
struct Prefix {
    /// Each exemplar, as a turn of its file-wide speaker.
    spans: Vec<SpeakerTurn>,
    /// The diarizer's turns there, with the window's own speaker ids.
    turns: Vec<SpeakerTurn>,
}

/// Links the window speakers that `links` (the overlap's) left out to the
/// speakers of the exemplars they were heard in, the same way as in the
/// overlap. Returns new id → old id.
fn link_exemplars(prefix: &Prefix, links: &HashMap<u32, u32>) -> HashMap<u32, u32> {
    let spans: Vec<SpeakerTurn> = prefix
        .spans
        .iter()
        .filter(|span| !links.values().any(|&old| old == span.speaker))
        .copied()
        .collect();
    let heard: Vec<SpeakerTurn> = prefix
        .turns
        .iter()
        .filter(|turn| !links.contains_key(&turn.speaker))
        .copied()
        .collect();
    link_speakers(&spans, &heard, 0, u64::MAX, EXEMPLAR_LINK_MIN_MS)
}

/// Splits a run's turns into those of its first `prefix_ms` (the
/// exemplars) and those after it, timed from the end of the prefix.
fn split_prefix(turns: &[SpeakerTurn], prefix_ms: u64) -> (Vec<SpeakerTurn>, Vec<SpeakerTurn>) {
    let body = clip(turns, prefix_ms, u64::MAX)
        .into_iter()
        .map(|turn| SpeakerTurn {
            start_ms: turn.start_ms - prefix_ms,
            end_ms: turn.end_ms - prefix_ms,
            ..turn
        })
        .collect();
    (clip(turns, 0, prefix_ms), body)
}

/// A few seconds of clean speech of each file-wide speaker, from the windows
/// diarized so far. At most `MAX_EXEMPLARS` of `EXEMPLAR_MAX_MS` each.
#[derive(Default)]
struct Exemplars {
    /// File-wide id → exemplar audio.
    audio: HashMap<u32, Vec<f32>>,
    /// File-wide id → how long the speaker has talked so far.
    speech: HashMap<u32, u64>,
}

impl Exemplars {
    /// The speakers whose exemplars go before the next window, given how
    /// long each speaker talks in its overlap: those not heard enough there
    /// to be linked by it, as long as the run then holds no more voices
    /// than the model tells apart. Speakers heard in the overlap come first
    /// (they add no voice), then those who talked most.
    fn choose(&self, overlap_speech: &HashMap<u32, u64>) -> Vec<u32> {
        let mut candidates: Vec<u32> = self
            .audio
            .keys()
            .copied()
            .filter(|id| {
                overlap_speech
                    .get(id)
                    .is_none_or(|&ms| ms < EXEMPLAR_CLEAR_MS)
            })
            .collect();
        candidates.sort_by_key(|id| {
            (
                !overlap_speech.contains_key(id),
                std::cmp::Reverse(self.speech.get(id).copied().unwrap_or(0)),
                *id,
            )
        });
        let mut voices: Vec<u32> = overlap_speech.keys().copied().collect();
        let mut chosen = Vec::new();
        for id in candidates {
            if voices.contains(&id) {
                chosen.push(id);
            } else if voices.len() < MODEL_SPEAKERS {
                voices.push(id);
                chosen.push(id);
            }
        }
        chosen
    }

    /// The audio to diarize before a window: each chosen exemplar followed
    /// by `EXEMPLAR_GAP_MS` of silence, and where each exemplar lies in it.
    fn prefix(&self, chosen: &[u32]) -> (Vec<f32>, Vec<SpeakerTurn>) {
        let gap = (EXEMPLAR_GAP_MS * SAMPLES_PER_MS) as usize;
        let mut audio = Vec::new();
        let mut spans = Vec::new();
        for id in chosen {
            let Some(exemplar) = self.audio.get(id) else {
                continue;
            };
            let start_ms = audio.len() as u64 / SAMPLES_PER_MS;
            audio.extend_from_slice(exemplar);
            spans.push(SpeakerTurn {
                start_ms,
                end_ms: audio.len() as u64 / SAMPLES_PER_MS,
                speaker: *id,
            });
            audio.resize(audio.len() + gap, 0.0);
        }
        (audio, spans)
    }

    /// Takes exemplars from a window (`samples`, starting at `start_ms`),
    /// given its `turns` in file time and its id → file-wide id. A new
    /// exemplar replaces an older one at least as long, so they stay fresh.
    fn update(
        &mut self,
        samples: &[f32],
        start_ms: u64,
        turns: &[SpeakerTurn],
        ids: &HashMap<u32, u32>,
    ) {
        let turns: Vec<SpeakerTurn> = turns
            .iter()
            .filter_map(|turn| {
                Some(SpeakerTurn {
                    start_ms: turn.start_ms.saturating_sub(start_ms),
                    end_ms: turn.end_ms.saturating_sub(start_ms),
                    speaker: *ids.get(&turn.speaker)?,
                })
            })
            .collect();
        for turn in &turns {
            *self.speech.entry(turn.speaker).or_default() += turn.end_ms - turn.start_ms;
        }
        for (speaker, pieces) in exemplar_pieces(&turns) {
            let ms: u64 = pieces.iter().map(|(from, to)| to - from).sum();
            if self
                .audio
                .get(&speaker)
                .is_some_and(|old| old.len() as u64 > ms * SAMPLES_PER_MS)
            {
                continue;
            }
            let mut audio = Vec::with_capacity((ms * SAMPLES_PER_MS) as usize);
            for (from, to) in pieces {
                let from = ((from * SAMPLES_PER_MS) as usize).min(samples.len());
                let to = ((to * SAMPLES_PER_MS) as usize).min(samples.len());
                audio.extend_from_slice(&samples[from..to]);
            }
            if audio.len() as u64 >= EXEMPLAR_MIN_MS * SAMPLES_PER_MS {
                self.audio.insert(speaker, audio);
            }
        }
        while self.audio.len() > MAX_EXEMPLARS {
            let Some(quietest) = self.audio.keys().copied().min_by_key(|id| {
                (
                    self.speech.get(id).copied().unwrap_or(0),
                    std::cmp::Reverse(*id),
                )
            }) else {
                break;
            };
            self.audio.remove(&quietest);
        }
    }
}

/// Where to cut each speaker's exemplar from `turns`: their longest turns
/// that no one else talks over, `EXEMPLAR_EDGE_MS` in from each end, until
/// `EXEMPLAR_TARGET_MS` (at most `EXEMPLAR_MAX_MS`). Speakers with less than
/// `EXEMPLAR_MIN_MS` of such speech get none. Pieces are in time order.
fn exemplar_pieces(turns: &[SpeakerTurn]) -> HashMap<u32, Vec<(u64, u64)>> {
    let merged = normalize_turns(turns, 300);
    let mut clean: Vec<SpeakerTurn> = merged
        .iter()
        .filter(|turn| {
            !merged.iter().any(|other| {
                other.speaker != turn.speaker
                    && other.start_ms < turn.end_ms
                    && turn.start_ms < other.end_ms
            })
        })
        .map(|turn| SpeakerTurn {
            start_ms: turn.start_ms + EXEMPLAR_EDGE_MS,
            end_ms: turn.end_ms.saturating_sub(EXEMPLAR_EDGE_MS),
            speaker: turn.speaker,
        })
        .filter(|turn| turn.end_ms >= turn.start_ms + EXEMPLAR_MIN_PIECE_MS)
        .collect();
    clean.sort_by_key(|turn| {
        (
            std::cmp::Reverse(turn.end_ms - turn.start_ms),
            turn.start_ms,
        )
    });

    let mut pieces: HashMap<u32, Vec<(u64, u64)>> = HashMap::new();
    for turn in clean {
        let taken = pieces.entry(turn.speaker).or_default();
        let ms: u64 = taken.iter().map(|(from, to)| to - from).sum();
        if ms >= EXEMPLAR_TARGET_MS {
            continue;
        }
        let end = turn.end_ms.min(turn.start_ms + (EXEMPLAR_MAX_MS - ms));
        taken.push((turn.start_ms, end));
    }
    pieces.retain(|_, taken| {
        taken.sort_unstable();
        taken.iter().map(|(from, to)| to - from).sum::<u64>() >= EXEMPLAR_MIN_MS
    });
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(start_ms: u64, end_ms: u64, speaker: u32) -> SpeakerTurn {
        SpeakerTurn {
            start_ms,
            end_ms,
            speaker,
        }
    }

    fn row(t0_ms: i64, t1_ms: i64, speaker_id: i32) -> SpeakerSegment {
        SpeakerSegment {
            t0_ms,
            t1_ms,
            speaker_id,
            p: f32::NAN,
        }
    }

    #[test]
    fn rows_to_turns_skips_invalid() {
        let rows = [
            row(0, 1_760, 1),
            row(8_560, 9_040, 4),
            row(500, 500, 1),  // no duration
            row(900, 800, 1),  // inverted
            row(100, 200, 0),  // no speaker
            row(100, 200, -1), // no speaker
            row(-40, 300, 2),  // starts before the audio
            row(-90, -10, 2),  // entirely before it
        ];
        assert_eq!(
            rows_to_turns(&rows),
            vec![turn(0, 1_760, 1), turn(8_560, 9_040, 4), turn(0, 300, 2)]
        );
    }

    #[test]
    fn error_states() {
        assert_eq!(
            DiarizeError::ModelMissing(PathBuf::from("/m.gguf")).job_state(),
            SpeakerJobState::Skipped {
                reason: SpeakerSkipReason::ModelMissing
            }
        );
        assert_eq!(
            DiarizeError::Cancelled.job_state(),
            SpeakerJobState::Failed {
                message: "cancelled".to_string()
            }
        );
        assert_eq!(
            DiarizeError::TooLong {
                duration_ms: 13 * 3_600_000
            }
            .job_state(),
            SpeakerJobState::Failed {
                message: "the recording is too long to label speakers (780 min)".to_string()
            }
        );
        assert_eq!(
            DiarizeError::Engine("worker crashed".to_string()).job_state(),
            SpeakerJobState::Failed {
                message: "the speaker model failed: worker crashed".to_string()
            }
        );
        assert_eq!(
            DiarizeError::from(EngineError::Cancelled),
            DiarizeError::Cancelled
        );
        assert_eq!(
            DiarizeError::from(EngineError::Failed("boom".to_string())),
            DiarizeError::Engine("boom".to_string())
        );
    }

    fn limits(window_ms: u64, overlap_ms: u64) -> Limits {
        Limits {
            window_ms,
            overlap_ms,
            max_ms: MAX_DIARIZE_MS,
        }
    }

    /// Feeds `total` samples (each sample's value is its position) in
    /// blocks of `block`; returns each window as (start, first, len) and
    /// the most samples ever buffered.
    fn windows_of(limits: Limits, total: usize, block: usize) -> (Vec<(u64, f32, usize)>, usize) {
        let mut windows = Windows::new(limits, None);
        let mut seen = Vec::new();
        let mut most = 0;
        let samples: Vec<f32> = (0..total).map(|i| i as f32).collect();
        let mut record = |start: u64, window: Vec<f32>, _more: bool| -> Result<(), DiarizeError> {
            seen.push((start, window[0], window.len()));
            Ok(())
        };
        for chunk in samples.chunks(block) {
            windows.push(chunk, &mut record).unwrap();
            most = most.max(windows.buffer.len());
        }
        windows.finish(&mut record).unwrap();
        (seen, most)
    }

    #[test]
    fn windows_overlap_and_bound_the_buffer() {
        // 10 ms windows (160 samples) overlapping by 2 ms (32 samples).
        let limits = limits(10, 2);
        // Up to window + overlap: one window.
        assert_eq!(windows_of(limits, 192, 7).0, vec![(0, 0.0, 192)]);
        assert_eq!(windows_of(limits, 50, 7).0, vec![(0, 0.0, 50)]);
        assert!(windows_of(limits, 0, 7).0.is_empty());

        // Longer: windows every 128 samples, each 160 long, the last one
        // taking what is left (more than twice the overlap).
        let (windows, most) = windows_of(limits, 1_000, 7);
        let mut expected: Vec<(u64, f32, usize)> = (0..7u64)
            .map(|k| (k * 128, (k * 128) as f32, 160))
            .collect();
        expected.push((896, 896.0, 104));
        assert_eq!(windows, expected);
        assert!(most <= 160 + 32 + 7, "buffered {most}");
    }

    #[test]
    fn the_last_window_is_never_a_sliver() {
        let limits = limits(10, 2);
        for total in 193..700 {
            let (windows, _) = windows_of(limits, total, 13);
            let (start, _, len) = *windows.last().unwrap();
            assert!(len > 64, "{total}: last window of {len}");
            assert_eq!(start as usize + len, total);
        }
    }

    #[test]
    fn link_speakers_by_co_occurrence() {
        // Previous window: A=1 talks 0–40 s, B=2 40–90 s. The new window
        // calls B "1" and A "3", and has a newcomer "2" after the overlap.
        let previous = [turn(0, 40_000, 1), turn(40_000, 90_000, 2)];
        let next = [
            turn(0, 40_000, 3),
            turn(40_000, 90_000, 1),
            turn(95_000, 99_000, 2),
        ];
        let links = link_speakers(&previous, &next, 0, 90_000, LINK_MIN_MS);
        assert_eq!(links, HashMap::from([(3, 1), (1, 2)]));
    }

    #[test]
    fn link_needs_3s_and_half_of_the_quieter_speech() {
        // 2.5 s together: too little.
        let links = link_speakers(
            &[turn(0, 2_500, 1)],
            &[turn(0, 2_500, 4)],
            0,
            90_000,
            LINK_MIN_MS,
        );
        assert!(links.is_empty());
        // 4 s together, but the new speaker talks 20 s in the overlap and
        // the old one 30 s: 4 s is under half of 20 s.
        let links = link_speakers(
            &[turn(0, 30_000, 1)],
            &[turn(26_000, 46_000, 4)],
            0,
            90_000,
            LINK_MIN_MS,
        );
        assert!(links.is_empty());
        // 12 s together of 20: linked.
        let links = link_speakers(
            &[turn(0, 30_000, 1)],
            &[turn(18_000, 38_000, 4)],
            0,
            90_000,
            LINK_MIN_MS,
        );
        assert_eq!(links, HashMap::from([(4, 1)]));
        // Only the overlap counts.
        let links = link_speakers(
            &[turn(0, 100_000, 1)],
            &[turn(0, 100_000, 2)],
            95_000,
            97_000,
            LINK_MIN_MS,
        );
        assert!(links.is_empty());
    }

    #[test]
    fn links_are_one_to_one() {
        // Both new speakers overlap old speaker 1; the larger one wins it
        // and the other stays unlinked.
        let previous = [turn(0, 60_000, 1)];
        let next = [turn(0, 40_000, 2), turn(40_000, 60_000, 3)];
        let links = link_speakers(&previous, &next, 0, 90_000, LINK_MIN_MS);
        assert_eq!(links, HashMap::from([(2, 1)]));
    }

    #[test]
    fn stitcher_hands_over_at_the_overlap_midpoint() {
        // Window 1: 0–100 s; window 2: 60–160 s (overlap 60–100, cut 80).
        let mut stitcher = Stitcher::default();
        stitcher.add(
            0,
            100_000,
            vec![turn(0, 50_000, 4), turn(50_000, 100_000, 1)],
            &Prefix::default(),
        );
        // The second window calls the speaker of 50–100 s "2", and has a
        // new one ("3") from 120 s.
        stitcher.add(
            60_000,
            160_000,
            vec![turn(60_000, 110_000, 2), turn(120_000, 160_000, 3)],
            &Prefix::default(),
        );
        let turns = normalize_turns(&stitcher.finish(), 300);
        assert_eq!(
            turns,
            vec![
                turn(0, 50_000, 1),
                turn(50_000, 110_000, 2),
                turn(120_000, 160_000, 3),
            ]
        );
    }

    #[test]
    fn stitcher_keeps_ids_through_many_windows() {
        // Two speakers alternating every 10 s for 10 windows; each window
        // numbers them differently. Windows of 100 s every 80 s.
        let speaker_at = |ms: u64| if (ms / 10_000) % 2 == 0 { "A" } else { "B" };
        let mut stitcher = Stitcher::default();
        for w in 0..10u64 {
            let start = w * 80_000;
            let end = start + 100_000;
            let local = |name: &str| match (name, w % 2) {
                ("A", 0) | ("B", 1) => 7,
                _ => 2,
            };
            let turns = (start / 10_000..end / 10_000)
                .map(|slot| {
                    turn(
                        slot * 10_000,
                        (slot + 1) * 10_000,
                        local(speaker_at(slot * 10_000)),
                    )
                })
                .collect();
            stitcher.add(start, end, turns, &Prefix::default());
        }
        let turns = normalize_turns(&stitcher.finish(), 300);
        let mut ids: HashMap<&str, u32> = HashMap::new();
        for t in &turns {
            let name = speaker_at(t.start_ms);
            assert_eq!(*ids.entry(name).or_insert(t.speaker), t.speaker, "{t:?}");
            assert_eq!(t.end_ms - t.start_ms, 10_000, "{t:?}");
        }
        assert_eq!(ids.len(), 2);
        // Contiguous, without duplicates.
        assert_eq!(turns.len(), 82);
        for pair in turns.windows(2) {
            assert_eq!(pair[0].end_ms, pair[1].start_ms);
        }
    }

    /// Window 1 (0–100 s): A talks 0–30 s, B 30–50 s, A again 50–100 s.
    /// Window 2 (60–160 s) calls A "3" and B "5"; only A talks in the
    /// overlap (60–100 s), so only an exemplar of B can link B.
    fn add_two_windows(prefix: &Prefix) -> (HashMap<u32, u32>, Vec<SpeakerTurn>) {
        let mut stitcher = Stitcher::default();
        let first = stitcher.add(
            0,
            100_000,
            vec![
                turn(0, 30_000, 1),
                turn(30_000, 50_000, 2),
                turn(50_000, 100_000, 1),
            ],
            &Prefix::default(),
        );
        assert_eq!(first, HashMap::from([(1, 1), (2, 2)]));
        let ids = stitcher.add(
            60_000,
            160_000,
            vec![turn(60_000, 110_000, 3), turn(110_000, 160_000, 5)],
            prefix,
        );
        (ids, normalize_turns(&stitcher.finish(), 300))
    }

    #[test]
    fn a_speaker_silent_in_the_overlap_keeps_their_id_through_an_exemplar() {
        // B's exemplar (file-wide id 2) went before window 2, and the
        // diarizer heard it as its speaker "5".
        let prefix = Prefix {
            spans: vec![turn(0, 8_000, 2)],
            turns: vec![turn(100, 7_900, 5)],
        };
        let (ids, turns) = add_two_windows(&prefix);
        assert_eq!(ids, HashMap::from([(3, 1), (5, 2)]));
        assert_eq!(
            turns,
            vec![
                turn(0, 30_000, 1),
                turn(30_000, 50_000, 2),
                turn(50_000, 110_000, 1),
                turn(110_000, 160_000, 2),
            ]
        );

        // Without the exemplar B would become a third speaker.
        let (ids, _) = add_two_windows(&Prefix::default());
        assert_eq!(ids, HashMap::from([(3, 1), (5, 3)]));
    }

    #[test]
    fn exemplars_link_only_what_the_overlap_left() {
        // The overlap linked the window's "3" to 1. Exemplars of 1 and 2.
        let links = HashMap::from([(3, 1)]);
        let spans = vec![turn(0, 8_000, 1), turn(9_000, 17_000, 2)];
        // 1's exemplar heard as "4": 1 is taken, so no link. 2's heard as
        // "3" (already linked) and, for 1 s, as "5": too little.
        let prefix = Prefix {
            spans: spans.clone(),
            turns: vec![
                turn(0, 8_000, 4),
                turn(9_000, 16_000, 3),
                turn(16_000, 17_000, 5),
            ],
        };
        assert!(link_exemplars(&prefix, &links).is_empty());
        // Heard as "5" for 6 of its 8 s: linked.
        let prefix = Prefix {
            spans,
            turns: vec![turn(9_000, 11_000, 3), turn(11_000, 17_000, 5)],
        };
        assert_eq!(link_exemplars(&prefix, &links), HashMap::from([(5, 2)]));
    }

    #[test]
    fn split_prefix_drops_the_exemplars_and_shifts_back() {
        let turns = [
            turn(0, 4_000, 1),
            turn(5_000, 7_000, 2),
            turn(6_500, 9_000, 3),
        ];
        let (prefix, body) = split_prefix(&turns, 6_000);
        assert_eq!(prefix, vec![turn(0, 4_000, 1), turn(5_000, 6_000, 2)]);
        assert_eq!(body, vec![turn(0, 1_000, 2), turn(500, 3_000, 3)]);
        let (prefix, body) = split_prefix(&turns, 0);
        assert!(prefix.is_empty());
        assert_eq!(body, turns);
    }

    #[test]
    fn exemplar_pieces_take_the_longest_clean_turns() {
        let turns = [
            turn(0, 4_000, 1),
            turn(4_500, 6_000, 1),
            turn(6_000, 20_000, 2),
            turn(20_000, 26_000, 1),
            turn(26_000, 27_000, 3),
            // Talked over: neither is clean.
            turn(30_000, 32_000, 1),
            turn(31_000, 45_000, 4),
            // Clean, but only 2.1 s once the edges are cut.
            turn(50_000, 52_500, 5),
        ];
        let pieces = exemplar_pieces(&turns);
        assert_eq!(
            pieces,
            HashMap::from([
                // 5.6 s, then 3.6 s: past 8 s, so the 1.1 s piece is left.
                (1, vec![(200, 3_800), (20_200, 25_800)]),
                // 13.6 s of clean speech, cut to 10 s.
                (2, vec![(6_200, 16_200)]),
            ])
        );
    }

    #[test]
    fn exemplars_go_to_who_the_overlap_may_miss() {
        let mut exemplars = Exemplars::default();
        for (id, speech_s) in [(1, 100), (2, 50), (3, 80), (4, 10), (5, 5)] {
            exemplars.audio.insert(id, vec![0.0; 16]);
            exemplars.speech.insert(id, speech_s * 1_000);
        }
        // 1 talks enough in the overlap to be linked there; 4 says "mm-hmm".
        let overlap = HashMap::from([(1, 40_000), (4, 1_000)]);
        // 4 first (already heard), then by speech, up to four voices.
        assert_eq!(exemplars.choose(&overlap), vec![4, 3, 2]);
        // A silent overlap: the four who talked most.
        assert_eq!(exemplars.choose(&HashMap::new()), vec![1, 3, 2, 4]);
    }

    #[test]
    fn exemplars_are_cut_from_the_window_and_bounded() {
        // Each sample holds its time in ms; the window starts at 60 s.
        let samples: Vec<f32> = (0..20_000 * 16).map(|i| (i / 16) as f32).collect();
        let mut exemplars = Exemplars::default();
        let ids = HashMap::from([(7, 2)]);
        exemplars.update(&samples, 60_000, &[turn(61_000, 66_000, 7)], &ids);
        let audio = &exemplars.audio[&2];
        assert_eq!(audio.len(), 4_600 * 16);
        assert_eq!((audio[0], audio[audio.len() - 1]), (1_200.0, 5_799.0));
        assert_eq!(exemplars.speech[&2], 5_000);

        let (prefix, spans) = exemplars.prefix(&[2, 9]);
        assert_eq!(prefix.len(), (4_600 + EXEMPLAR_GAP_MS as usize) * 16);
        assert_eq!(spans, vec![turn(0, 4_600, 2)]);
        assert!(prefix[4_600 * 16..].iter().all(|&s| s == 0.0));

        // A shorter one does not replace it; a longer one does.
        exemplars.update(&samples, 60_000, &[turn(70_000, 74_000, 7)], &ids);
        assert_eq!(exemplars.audio[&2][0], 1_200.0);
        exemplars.update(&samples, 60_000, &[turn(70_000, 79_000, 7)], &ids);
        assert_eq!(exemplars.audio[&2][0], 10_200.0);

        // Many speakers: the quietest lose their exemplars.
        for id in 10..40u32 {
            let ids = HashMap::from([(1, id)]);
            let end = 64_000 + u64::from(id) * 100;
            exemplars.update(&samples, 60_000, &[turn(60_000, end, 1)], &ids);
        }
        assert_eq!(exemplars.audio.len(), MAX_EXEMPLARS);
        assert!(exemplars.audio.contains_key(&2) && exemplars.audio.contains_key(&39));
        assert!(!exemplars.audio.contains_key(&10));
    }

    fn write_wav(path: &Path, seconds: f32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(seconds * 16_000.0) as usize {
            let s = ((i as f32 * 0.05).sin() * 8_000.0) as i16;
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn request(dir: &Path, estimated_duration_ms: Option<u64>) -> DiarizeRequest {
        let model_path = dir.join("model.gguf");
        std::fs::write(&model_path, b"not a model").unwrap();
        DiarizeRequest {
            path: dir.join("audio.wav"),
            decode: DecodeOptions::default(),
            model_path,
            estimated_duration_ms,
            limit_ms: None,
        }
    }

    fn diarization(limits: Limits) -> Diarization {
        Diarization {
            job: WorkToken::new(),
            token: WorkToken::new(),
            hold: Arc::new(|| false),
            engine: Arc::new(Mutex::new(None)),
            limits,
            progress: mpsc::channel().0,
        }
    }

    #[test]
    fn missing_model_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let req = DiarizeRequest {
            model_path: dir.path().join("absent.gguf"),
            ..request(dir.path(), None)
        };
        let result = start_diarization(req, WorkToken::new(), Arc::new(|| false)).wait();
        assert_eq!(
            result,
            Err(DiarizeError::ModelMissing(dir.path().join("absent.gguf")))
        );
    }

    #[test]
    fn too_long_is_rejected_before_decoding_finishes() {
        let dir = tempfile::tempdir().unwrap();
        // By the probe's estimate, before decoding at all.
        let req = request(dir.path(), Some(MAX_DIARIZE_MS + 1));
        assert_eq!(
            start_diarization(req, WorkToken::new(), Arc::new(|| false)).wait(),
            Err(DiarizeError::TooLong {
                duration_ms: MAX_DIARIZE_MS + 1
            })
        );
        // While decoding, when the estimate was missing or wrong: 3 s of
        // audio against a 1 s limit stops without starting a worker.
        let req = request(dir.path(), None);
        write_wav(&req.path, 3.0);
        let small = Limits {
            max_ms: 1_000,
            ..Limits::default()
        };
        match diarization(small).run(&req) {
            Err(DiarizeError::TooLong { duration_ms }) => {
                assert!(duration_ms > 1_000 && duration_ms <= 3_000, "{duration_ms}")
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn decode_errors_and_cancel_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path(), None);
        // The audio file does not exist.
        assert!(matches!(
            diarization(Limits::default()).run(&req),
            Err(DiarizeError::Decode(_))
        ));

        // A cancelled job stops before any worker starts.
        write_wav(&req.path, 1.0);
        let job = WorkToken::new();
        job.cancel();
        assert_eq!(
            start_diarization(req, job.clone(), Arc::new(|| false)).wait(),
            Err(DiarizeError::Cancelled)
        );
    }

    #[test]
    fn cancel_leaves_the_job_token_alone() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path(), None);
        write_wav(&req.path, 1.0);
        let job = WorkToken::new();
        // Held as if dictation never ends, so the run waits for the cancel.
        let handle = start_diarization(req, job.clone(), Arc::new(|| true));
        thread::sleep(Duration::from_millis(50));
        handle.cancel();
        assert_eq!(handle.wait(), Err(DiarizeError::Cancelled));
        assert!(!job.is_cancelled());
    }

    #[test]
    fn progress_reports_waiting_then_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path(), None);
        write_wav(&req.path, 1.0);
        let job = WorkToken::new();
        // Held as if dictation never ends. Once the hold has been asked
        // twice, the first ask's Waiting has been sent.
        let asked = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hold = {
            let asked = Arc::clone(&asked);
            Arc::new(move || {
                asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            })
        };
        let handle = start_diarization(req, job.clone(), hold);
        let started = Instant::now();
        while asked.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            assert!(started.elapsed() < Duration::from_secs(20), "never held");
            thread::sleep(Duration::from_millis(10));
        }
        handle.cancel();

        let mut seen = Vec::new();
        let result = handle.wait_reporting(&mut |progress| seen.push(progress));
        assert_eq!(result, Err(DiarizeError::Cancelled));
        assert_eq!(seen, [DiarizeProgress::Waiting]);
        assert!(!job.is_cancelled());
    }

    #[test]
    fn limit_stops_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let req = DiarizeRequest {
            limit_ms: Some(1_000),
            ..request(dir.path(), None)
        };
        write_wav(&req.path, 3.0);
        let mut windows: Vec<(u64, usize, bool)> = Vec::new();
        let mut record = |start: u64, samples: Vec<f32>, more: bool| -> Result<(), DiarizeError> {
            windows.push((start, samples.len(), more));
            Ok(())
        };
        diarization(Limits::default())
            .decode_windows(&req, None, &mut record)
            .expect("decode up to the limit");
        // One window of exactly the first second.
        assert_eq!(windows, [(0, 16_000, false)]);

        // Without a limit, the whole file.
        let mut windows: Vec<(u64, usize, bool)> = Vec::new();
        let mut record = |start: u64, samples: Vec<f32>, more: bool| -> Result<(), DiarizeError> {
            windows.push((start, samples.len(), more));
            Ok(())
        };
        let whole = DiarizeRequest {
            limit_ms: None,
            ..req
        };
        diarization(Limits::default())
            .decode_windows(&whole, None, &mut record)
            .expect("decode");
        assert_eq!(windows, [(0, 48_000, false)]);
    }

    #[test]
    fn limit_shortens_the_too_long_check() {
        let dir = tempfile::tempdir().unwrap();
        // A file estimated beyond the bound, but only its start is needed.
        let req = DiarizeRequest {
            limit_ms: Some(1_000),
            ..request(dir.path(), Some(MAX_DIARIZE_MS + 1))
        };
        // Not TooLong: it fails decoding instead (there is no audio file).
        assert!(matches!(
            diarization(Limits::default()).run(&req),
            Err(DiarizeError::Decode(_))
        ));
    }

    // ---- With the real model ----------------------------------------------

    /// The utterances of dialog2.wav (two synthetic voices), in seconds.
    const DIALOG2_TRUTH: &[(f64, f64)] = &[
        (0.0, 7.95),
        (8.55, 17.77),
        (18.37, 25.31),
        (25.91, 33.51),
        (34.11, 41.46),
        (42.06, 50.66),
        (51.26, 58.28),
        (58.88, 65.28),
    ];

    fn env_path(name: &str) -> PathBuf {
        PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")))
    }

    fn real_request(path: PathBuf) -> DiarizeRequest {
        DiarizeRequest {
            path,
            decode: DecodeOptions::default(),
            model_path: env_path("HANDY_SPEAKER_MODEL"),
            estimated_duration_ms: None,
            limit_ms: None,
        }
    }

    /// Who speaks most in `start..end` (s), and the share of that span.
    fn majority(turns: &[SpeakerTurn], start: f64, end: f64) -> (Option<u32>, f64) {
        let (start, end) = ((start * 1000.0) as u64, (end * 1000.0) as u64);
        let mut by_speaker: HashMap<u32, u64> = HashMap::new();
        for turn in turns {
            let shared = turn
                .end_ms
                .min(end)
                .saturating_sub(turn.start_ms.max(start));
            *by_speaker.entry(turn.speaker).or_default() += shared;
        }
        by_speaker
            .into_iter()
            .filter(|&(_, ms)| ms > 0)
            .max_by_key(|&(speaker, ms)| (ms, std::cmp::Reverse(speaker)))
            .map_or((None, 0.0), |(speaker, ms)| {
                (Some(speaker), ms as f64 / (end - start) as f64)
            })
    }

    /// `HANDY_TRANSCRIBE_WORKER_EXE=target/debug/handy`
    /// `HANDY_SPEAKER_MODEL=<…Q8_0.gguf> HANDY_DIARIZE_WAV=<dialog2.wav>`
    /// `cargo test --lib diarize -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore]
    fn diarizes_two_voices_with_the_real_model() {
        let started = Instant::now();
        let turns = start_diarization(
            real_request(env_path("HANDY_DIARIZE_WAV")),
            WorkToken::new(),
            Arc::new(|| false),
        )
        .wait()
        .expect("diarize");
        println!("diarized in {:?}", started.elapsed());
        let turns = normalize_turns(&turns, 300);
        let ids: std::collections::BTreeSet<u32> = turns.iter().map(|t| t.speaker).collect();
        assert_eq!(ids.len(), 2, "{turns:?}");
        // Eight alternating turns, each boundary within 1 s of the truth.
        let first = turns[0].speaker;
        let mut changes: Vec<SpeakerTurn> = Vec::new();
        for turn in &turns {
            match changes.last_mut() {
                Some(last) if last.speaker == turn.speaker => last.end_ms = turn.end_ms,
                _ => changes.push(*turn),
            }
        }
        assert_eq!(changes.len(), DIALOG2_TRUTH.len(), "{changes:?}");
        for (i, (turn, &(start, end))) in changes.iter().zip(DIALOG2_TRUTH).enumerate() {
            assert_eq!(turn.speaker == first, i % 2 == 0, "{turn:?}");
            assert!(
                (turn.start_ms as f64 / 1000.0 - start).abs() < 1.0,
                "{turn:?}"
            );
            assert!((turn.end_ms as f64 / 1000.0 - end).abs() < 1.0, "{turn:?}");
        }
    }

    /// A long recording made of `HANDY_DIARIZE_LONG_REPEATS` copies of
    /// dialog2.wav back to back (`HANDY_DIARIZE_LONG_WAV`): several windows,
    /// whose speaker ids must stay the same from the first to the last.
    #[test]
    #[ignore]
    fn windowed_ids_stay_consistent_over_a_long_file() {
        let repeats: usize = std::env::var("HANDY_DIARIZE_LONG_REPEATS")
            .expect("set HANDY_DIARIZE_LONG_REPEATS")
            .parse()
            .expect("a number");
        let period: f64 = std::env::var("HANDY_DIARIZE_LONG_PERIOD_S")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(65.877625);
        let started = Instant::now();
        let turns = start_diarization(
            real_request(env_path("HANDY_DIARIZE_LONG_WAV")),
            WorkToken::new(),
            Arc::new(|| false),
        )
        .wait()
        .expect("diarize");
        let elapsed = started.elapsed();
        let turns = normalize_turns(&turns, 300);
        let ids: std::collections::BTreeSet<u32> = turns.iter().map(|t| t.speaker).collect();

        let mut names: HashMap<usize, HashMap<Option<u32>, usize>> = HashMap::new();
        let mut covered = 0.0;
        let total = repeats * DIALOG2_TRUTH.len();
        for copy in 0..repeats {
            for (i, &(start, end)) in DIALOG2_TRUTH.iter().enumerate() {
                let offset = copy as f64 * period;
                let (speaker, share) = majority(&turns, start + offset, end + offset);
                covered += share;
                *names.entry(i % 2).or_default().entry(speaker).or_default() += 1;
            }
        }
        // Each voice's most common id, and how often its utterances got it.
        let mut correct = 0;
        for votes in names.values() {
            correct += votes.values().max().copied().unwrap_or(0);
        }
        println!(
            "{} s of audio in {:.1}s ({:.1}x real time); ids {:?}; utterances with \
             their voice's id: {}/{}; mean share of an utterance under its majority id: {:.3}; \
             votes {:?}",
            (repeats as f64 * period).round(),
            elapsed.as_secs_f64(),
            repeats as f64 * period / elapsed.as_secs_f64(),
            ids,
            correct,
            total,
            covered / total as f64,
            names
        );
        assert!(correct * 100 >= total * 95, "{correct}/{total}");
    }

    /// The same long recording (`HANDY_DIARIZE_LONG_WAV`): the progress
    /// reported never goes back and ends at the file's length.
    #[test]
    #[ignore]
    fn progress_follows_the_windows_with_the_real_model() {
        let path = env_path("HANDY_DIARIZE_LONG_WAV");
        let length_ms = hound::WavReader::open(&path)
            .map(|wav| u64::from(wav.duration()) * 1000 / u64::from(wav.spec().sample_rate))
            .expect("read the WAV header");
        let mut reported = Vec::new();
        start_diarization(real_request(path), WorkToken::new(), Arc::new(|| false))
            .wait_reporting(&mut |progress| {
                if let DiarizeProgress::Diarized { through_ms } = progress {
                    reported.push(through_ms);
                }
            })
            .expect("diarize");
        println!("progress: {reported:?} of {length_ms} ms");
        assert!(!reported.is_empty());
        assert!(
            reported.windows(2).all(|pair| pair[0] <= pair[1]),
            "{reported:?}"
        );
        let last = reported.last().copied().unwrap_or(0);
        assert!(last.abs_diff(length_ms) <= 1, "{last} of {length_ms}");
    }

    /// `HANDY_DIARIZE_WAV` and its truth (`HANDY_DIARIZE_TRUTH`: a JSON list
    /// of {speaker, start, end} in seconds), repeated every
    /// `HANDY_DIARIZE_PERIOD_S` seconds if set: prints how well the labels
    /// match. A measurement, not a pass/fail check.
    #[test]
    #[ignore]
    fn measure_against_truth() {
        measure();
    }

    /// The same, on a recording where one voice is silent through a window
    /// overlap (e.g. 45 min of dialog2.wav's two voices, with only one of
    /// them from 1050 to 1290 s): that voice must keep its id after it.
    #[test]
    #[ignore]
    fn a_speaker_silent_through_an_overlap_keeps_their_id_with_the_real_model() {
        let (ids, correct, total) = measure();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(correct * 100 >= total * 95, "{correct}/{total}");
    }

    /// Diarizes `HANDY_DIARIZE_WAV` and prints how well it matches its
    /// truth. Returns the ids found and how many utterances got their
    /// speaker's own id, of how many.
    fn measure() -> (std::collections::BTreeSet<u32>, usize, usize) {
        #[derive(serde::Deserialize)]
        struct Truth {
            speaker: String,
            start: f64,
            end: f64,
        }
        let truth: Vec<Truth> = serde_json::from_slice(
            &std::fs::read(env_path("HANDY_DIARIZE_TRUTH")).expect("read truth"),
        )
        .expect("parse truth");
        let period: Option<f64> = std::env::var("HANDY_DIARIZE_PERIOD_S")
            .ok()
            .and_then(|p| p.parse().ok());
        let started = Instant::now();
        let turns = start_diarization(
            real_request(env_path("HANDY_DIARIZE_WAV")),
            WorkToken::new(),
            Arc::new(|| false),
        )
        .wait()
        .expect("diarize");
        let elapsed = started.elapsed();
        let turns = normalize_turns(&turns, 300);
        let end_s = turns.last().map_or(0.0, |t| t.end_ms as f64 / 1000.0);
        let copies = period.map_or(1, |p| (end_s / p).round().max(1.0) as usize);

        let mut votes: HashMap<&str, HashMap<Option<u32>, usize>> = HashMap::new();
        let mut per_copy: Vec<Vec<Option<u32>>> = Vec::new();
        for copy in 0..copies {
            let offset = copy as f64 * period.unwrap_or(0.0);
            let mut row = Vec::new();
            for utterance in &truth {
                let (speaker, _) =
                    majority(&turns, utterance.start + offset, utterance.end + offset);
                *votes
                    .entry(utterance.speaker.as_str())
                    .or_default()
                    .entry(speaker)
                    .or_default() += 1;
                row.push(speaker);
            }
            per_copy.push(row);
        }
        // Each true speaker gets a distinct id, greedily by votes; an
        // utterance is right when its majority id is its speaker's.
        let mut pairs: Vec<(usize, &str, Option<u32>)> = votes
            .iter()
            .flat_map(|(name, v)| v.iter().map(|(id, n)| (*n, *name, *id)))
            .collect();
        pairs.sort_by_key(|&(n, name, id)| (std::cmp::Reverse(n), name, id));
        let (mut correct, mut named, mut taken) = (0, Vec::new(), Vec::new());
        for (n, name, id) in pairs {
            if id.is_some() && !named.contains(&name) && !taken.contains(&id) {
                correct += n;
                named.push(name);
                taken.push(id);
            }
        }
        let ids: std::collections::BTreeSet<u32> = turns.iter().map(|t| t.speaker).collect();
        println!(
            "{:.0} s in {:.1}s ({:.1}x real time); {} copies; ids {:?}; utterances whose \
             majority id is their speaker's own (one id per speaker): {}/{}",
            end_s,
            elapsed.as_secs_f64(),
            end_s / elapsed.as_secs_f64(),
            copies,
            ids,
            correct,
            copies * truth.len()
        );
        println!("votes per true speaker: {votes:?}");
        for (copy, row) in per_copy.iter().enumerate().take(3) {
            println!("copy {copy}: {row:?}");
        }
        (ids, correct, copies * truth.len())
    }

    /// Cancels a run in progress: the worker is killed and `wait` returns
    /// within 2 s.
    #[test]
    #[ignore]
    fn cancel_kills_the_diarizer_worker() {
        let job = WorkToken::new();
        let handle = start_diarization(
            real_request(env_path("HANDY_DIARIZE_WAV")),
            job.clone(),
            Arc::new(|| false),
        );
        thread::sleep(Duration::from_secs(1));
        job.cancel();
        let cancelled = Instant::now();
        assert_eq!(handle.wait(), Err(DiarizeError::Cancelled));
        assert!(
            cancelled.elapsed() < Duration::from_secs(2),
            "{:?}",
            cancelled.elapsed()
        );
    }
}
