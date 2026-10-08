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

use crate::engine_supervisor::{
    DeviceSelector, EngineError, EngineSupervisor, LoadSpec, WorkToken, WorkerOptions,
};
use crate::file_transcription::decode::{decode_to_16k_mono, DecodeError, DecodeOptions};
use crate::file_transcription::speakers::SpeakerTurn;
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
    let context = Diarization {
        job: token.clone(),
        token: own.clone(),
        hold,
        engine: Arc::clone(&engine),
        limits: Limits::default(),
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
    }
}

impl DiarizationHandle {
    /// Blocks until the diarization ends. Polls the job's token every
    /// DIARIZE_POLL and, on cancel, kills only the diarizer worker.
    pub fn wait(mut self) -> Result<Vec<SpeakerTurn>, DiarizeError> {
        let Some(thread) = self.thread.take() else {
            return Err(DiarizeError::Engine(
                "the speaker labelling thread could not start".to_string(),
            ));
        };
        let mut cancelled = false;
        while let Err(mpsc::RecvTimeoutError::Timeout) = self.done.recv_timeout(DIARIZE_POLL) {
            if !cancelled && self.job.is_cancelled() {
                self.cancel();
                cancelled = true;
            }
        }
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
}

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
        if let Some(duration_ms) = req
            .estimated_duration_ms
            .filter(|&ms| ms > self.limits.max_ms)
        {
            return Err(DiarizeError::TooLong { duration_ms });
        }

        let max_samples = self.limits.max_ms * SAMPLES_PER_MS;
        let mut windows = Windows::new(self.limits, req.estimated_duration_ms);
        let mut stitcher = Stitcher::default();
        let mut engine: Option<EngineSupervisor> = None;
        let mut run_window = |start: u64, samples: Vec<f32>| -> Result<(), DiarizeError> {
            self.run_window(req, &mut engine, &mut stitcher, start, samples)
        };

        let mut failure: Option<DiarizeError> = None;
        let mut decoded: u64 = 0;
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
                decoded += block.len() as u64;
                let step = if decoded > max_samples {
                    Err(DiarizeError::TooLong {
                        duration_ms: decoded / SAMPLES_PER_MS,
                    })
                } else {
                    windows.push(block, &mut run_window)
                };
                step.map_err(|e| {
                    let message = anyhow::anyhow!("speaker labelling stopped: {e}");
                    failure = Some(e);
                    message
                })
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
            Err(e) => return Err(DiarizeError::Decode(e.to_string())),
        }
        if self.stopped() {
            return Err(DiarizeError::Cancelled);
        }
        windows.finish(&mut run_window)?;
        Ok(stitcher.finish())
    }

    /// Diarizes one window (`samples`, starting `start` samples into the
    /// file) once dictation is idle, and links its speakers.
    fn run_window(
        &self,
        req: &DiarizeRequest,
        engine: &mut Option<EngineSupervisor>,
        stitcher: &mut Stitcher,
        start: u64,
        samples: Vec<f32>,
    ) -> Result<(), DiarizeError> {
        let mut held = false;
        while (self.hold)() {
            if self.stopped() {
                return Err(DiarizeError::Cancelled);
            }
            if !held {
                debug!("Speaker labelling waits while dictation is active");
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
        let started = Instant::now();
        let turns = diarize_samples(engine, &req.model_path, samples, &self.token)?;
        debug!(
            "Diarized {}–{} s of {} in {:.1}s: {} turns",
            start_ms / 1000,
            end_ms / 1000,
            req.path.display(),
            started.elapsed().as_secs_f64(),
            turns.len()
        );
        let turns = turns
            .into_iter()
            .map(|turn| SpeakerTurn {
                start_ms: turn.start_ms + start_ms,
                end_ms: turn.end_ms + start_ms,
                ..turn
            })
            .collect();
        stitcher.add(start_ms, end_ms, turns);
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

type RunWindow<'a> = dyn FnMut(u64, Vec<f32>) -> Result<(), DiarizeError> + 'a;

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
            run(self.start, window)?;
            self.start += stride as u64;
        }
        Ok(())
    }

    /// Runs the last window, if any audio is left.
    fn finish(self, run: &mut RunWindow<'_>) -> Result<(), DiarizeError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        run(self.start, self.buffer)
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
    /// `turns` are in file time with the window's own speaker ids.
    fn add(&mut self, start_ms: u64, end_ms: u64, turns: Vec<SpeakerTurn>) {
        let (links, cut_ms) = if self.windows == 0 {
            (HashMap::new(), start_ms)
        } else {
            let overlap_end = self.pending_end_ms.max(start_ms);
            (
                link_speakers(&self.pending, &turns, start_ms, overlap_end),
                start_ms + (overlap_end - start_ms) / 2,
            )
        };
        if self.windows > 0 {
            debug!(
                "Speaker ids of the window from {} s linked as {:?}",
                start_ms / 1000,
                links
            );
        }

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
/// least `LINK_MIN_MS` and `LINK_MIN_SHARE_PERCENT` of the quieter one's
/// speech there. Each id is linked at most once. Returns new id → old id.
fn link_speakers(
    previous: &[SpeakerTurn],
    next: &[SpeakerTurn],
    from: u64,
    to: u64,
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
        if ms >= LINK_MIN_MS && ms * 100 >= quieter * LINK_MIN_SHARE_PERCENT {
            links.insert(new, old);
            linked_old.push(old);
        }
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::speakers::normalize_turns;

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
        let mut record = |start: u64, window: Vec<f32>| -> Result<(), DiarizeError> {
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
        let links = link_speakers(&previous, &next, 0, 90_000);
        assert_eq!(links, HashMap::from([(3, 1), (1, 2)]));
    }

    #[test]
    fn link_needs_3s_and_half_of_the_quieter_speech() {
        // 2.5 s together: too little.
        let links = link_speakers(&[turn(0, 2_500, 1)], &[turn(0, 2_500, 4)], 0, 90_000);
        assert!(links.is_empty());
        // 4 s together, but the new speaker talks 20 s in the overlap and
        // the old one 30 s: 4 s is under half of 20 s.
        let links = link_speakers(&[turn(0, 30_000, 1)], &[turn(26_000, 46_000, 4)], 0, 90_000);
        assert!(links.is_empty());
        // 12 s together of 20: linked.
        let links = link_speakers(&[turn(0, 30_000, 1)], &[turn(18_000, 38_000, 4)], 0, 90_000);
        assert_eq!(links, HashMap::from([(4, 1)]));
        // Only the overlap counts.
        let links = link_speakers(
            &[turn(0, 100_000, 1)],
            &[turn(0, 100_000, 2)],
            95_000,
            97_000,
        );
        assert!(links.is_empty());
    }

    #[test]
    fn links_are_one_to_one() {
        // Both new speakers overlap old speaker 1; the larger one wins it
        // and the other stays unlinked.
        let previous = [turn(0, 60_000, 1)];
        let next = [turn(0, 40_000, 2), turn(40_000, 60_000, 3)];
        let links = link_speakers(&previous, &next, 0, 90_000);
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
        );
        // The second window calls the speaker of 50–100 s "2", and has a
        // new one ("3") from 120 s.
        stitcher.add(
            60_000,
            160_000,
            vec![turn(60_000, 110_000, 2), turn(120_000, 160_000, 3)],
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
            stitcher.add(start, end, turns);
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
        }
    }

    fn diarization(limits: Limits) -> Diarization {
        Diarization {
            job: WorkToken::new(),
            token: WorkToken::new(),
            hold: Arc::new(|| false),
            engine: Arc::new(Mutex::new(None)),
            limits,
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

    /// `HANDY_DIARIZE_WAV` and its truth (`HANDY_DIARIZE_TRUTH`: a JSON list
    /// of {speaker, start, end} in seconds), repeated every
    /// `HANDY_DIARIZE_PERIOD_S` seconds if set: prints how well the labels
    /// match. A measurement, not a pass/fail check.
    #[test]
    #[ignore]
    fn measure_against_truth() {
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
