//! Probing and streaming any supported file as 16 kHz mono f32.
//!
//! Three decoders, tried in this order:
//! 1. 16 kHz mono 16-bit PCM WAV is read directly with hound, giving exactly
//!    the samples `read_wav_samples` gives.
//! 2. symphonia, for every other symphonia-routed extension. Each packet is
//!    downmixed by plain average and resampled to 16 kHz as it arrives, so
//!    memory stays bounded whatever the file length.
//! 3. A system ffmpeg, for ffmpeg-routed extensions, and as the fallback when
//!    symphonia fails (or panics) before producing any audio.
//!
//! Once any audio has reached the sink, a later failure is reported as a
//! truncated `Ok`, never as an error (except cancel and sink failures), so
//! callers keep what was already transcribed.
//!
//! Both symphonia and ffmpeg keep the file's timeline: audio that starts
//! after the video, and stretches lost from the middle, come out as silence,
//! so that times in the output are times in the file.

use crate::file_transcription::{
    ffmpeg::{self, InputInfo},
    formats::{self, DecodeRoute},
    resample::StreamResampler,
    types::DecoderKind,
};
use log::debug;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{self as sc, CodecParameters, CodecType, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, Track};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Samples per sink call on the WAV fast path.
const WAV_BLOCK: usize = 64_000;
/// Consecutive undecodable packets after which a stream is given up.
const MAX_BAD_PACKETS: u32 = 64;
/// Timestamp gaps up to 100 ms (in 16 kHz samples) are left alone: codec
/// delay and rounding, not missing audio. The ffmpeg route does the same.
const MAX_IGNORED_GAP: u64 = 1_600;
/// A timestamp jump of more than an hour is a broken or restarted
/// timestamp, not a pause, and gets no silence.
const MAX_FILLED_GAP: u64 = 3_600 * 16_000;

#[derive(Clone, Debug, Default)]
pub struct DecodeOptions {
    /// Explicit ffmpeg (CLI --ffmpeg). None = find_ffmpeg(None).
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct AudioProbe {
    pub route: DecodeRoute,
    /// Best-known duration of the audio track (None if unknown).
    pub duration_ms: Option<u64>,
    /// The ffmpeg that would be used if the route or a fallback needs it.
    pub ffmpeg: Option<PathBuf>,
    pub source_size_bytes: u64,
    /// Unix seconds.
    pub source_modified_at: Option<i64>,
}

#[derive(Debug)]
pub enum DecodeError {
    /// Missing, not a regular file, or unreadable.
    NotFound(String),
    /// Unknown extension, no audio track, or undecodable with no ffmpeg fallback possible.
    Unsupported(String),
    /// Needs ffmpeg and none was found (the message names the extension/codec).
    NeedsFfmpeg(String),
    Cancelled,
    /// Failed before any audio reached the sink (after fallbacks), or the sink failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(detail) => f.write_str(detail),
            Self::Unsupported(detail) => write!(f, "unsupported file: {detail}"),
            Self::NeedsFfmpeg(detail) => write!(f, "{detail}, but ffmpeg was not found"),
            Self::Cancelled => f.write_str("decoding was cancelled"),
            Self::Failed(err) => write!(f, "decoding failed: {err:#}"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[derive(Clone, Debug)]
pub struct DecodeOutcome {
    pub decoder: DecoderKind,
    /// 16 kHz mono samples handed to the sink.
    pub samples: u64,
    /// Set when decoding stopped early after producing audio (truncated/corrupt tail).
    pub truncated: Option<String>,
}

/// Cheap: never decodes the whole file. symphonia track params (n_frames via
/// time_base), hound for WAV, ffmpeg::probe_duration_ms for the ffmpeg route
/// or when symphonia has no duration.
pub fn probe(path: &Path, opts: &DecodeOptions) -> Result<AudioProbe, DecodeError> {
    let metadata = check_source(path)?;
    let route = route_for(path)?;
    let ffmpeg = ffmpeg::find_ffmpeg(opts.ffmpeg.as_deref());

    let duration_ms = match route {
        DecodeRoute::Ffmpeg => {
            let ffmpeg = ffmpeg
                .as_deref()
                .ok_or_else(|| needs_ffmpeg_for_extension(path))?;
            probe_with_ffmpeg(ffmpeg, path)?
        }
        DecodeRoute::Symphonia => match fast_wav_duration_ms(path) {
            Some(duration_ms) => Some(duration_ms),
            None => match SymphoniaInput::open(path) {
                Ok(input) => input.duration_ms().or_else(|| {
                    ffmpeg
                        .as_deref()
                        .and_then(|ffmpeg| ffmpeg::probe_duration_ms(ffmpeg, path))
                }),
                Err(unusable) => {
                    debug!("symphonia cannot decode {}: {unusable}", path.display());
                    let Some(ffmpeg) = ffmpeg.as_deref() else {
                        return Err(unusable.into_error(path));
                    };
                    match unusable {
                        // symphonia saw nothing it could use: ffmpeg decides.
                        Unusable::Unrecognised(_)
                        | Unusable::Container(_)
                        | Unusable::NoAudioTrack => probe_with_ffmpeg(ffmpeg, path)?,
                        Unusable::Codec { duration_ms, .. } => {
                            duration_ms.or_else(|| ffmpeg::probe_duration_ms(ffmpeg, path))
                        }
                        Unusable::Corrupt(_) | Unusable::Empty => {
                            ffmpeg::probe_duration_ms(ffmpeg, path)
                        }
                    }
                }
            },
        },
    };

    Ok(AudioProbe {
        route,
        duration_ms,
        ffmpeg,
        source_size_bytes: metadata.len(),
        source_modified_at: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_secs()).ok()),
    })
}

/// Streams the whole file as 16 kHz mono f32 into `sink` (blocks of any size).
pub fn decode_to_16k_mono(
    path: &Path,
    opts: &DecodeOptions,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[f32]) -> anyhow::Result<()>,
) -> Result<DecodeOutcome, DecodeError> {
    check_source(path)?;
    let route = route_for(path)?;
    let mut sink = Sink::new(sink);

    if route == DecodeRoute::Ffmpeg {
        let ffmpeg = ffmpeg::find_ffmpeg(opts.ffmpeg.as_deref())
            .ok_or_else(|| needs_ffmpeg_for_extension(path))?;
        return match decode_ffmpeg(&ffmpeg, path, cancel, &mut sink) {
            Ok(outcome) => Ok(outcome),
            Err(Stop::Unusable(err)) => Err(DecodeError::Failed(err)),
            Err(Stop::Halted(err)) => Err(err),
        };
    }

    if is_wav(path) {
        if let Some(result) = decode_fast_wav(path, cancel, &mut sink) {
            return result;
        }
    }

    let unusable = match decode_symphonia(path, cancel, &mut sink) {
        Ok(outcome) => return Ok(outcome),
        Err(Stop::Unusable(unusable)) => unusable,
        Err(Stop::Halted(err)) => return Err(err),
    };
    // Nothing reached the sink, so ffmpeg can start from scratch.
    debug!(
        "symphonia cannot decode {}: {unusable}; trying ffmpeg",
        path.display()
    );
    let ffmpeg_error = match ffmpeg::find_ffmpeg(opts.ffmpeg.as_deref()) {
        Some(ffmpeg) => match decode_ffmpeg(&ffmpeg, path, cancel, &mut sink) {
            Ok(outcome) => return Ok(outcome),
            Err(Stop::Unusable(err)) => Some(err),
            Err(Stop::Halted(err)) => return Err(err),
        },
        None => None,
    };
    match (unusable, ffmpeg_error) {
        // The file says it holds no audio, and ffmpeg (which reads the WAV of
        // a crashed recorder to its end) found none either: an empty
        // recording (a header-only WAV), not an error.
        (Unusable::Empty, _) => Ok(sink.outcome(DecoderKind::Symphonia, None)),
        (unusable, None) => Err(unusable.into_error(path)),
        (unusable, Some(err)) => Err(unusable.with_ffmpeg_error(err)),
    }
}

fn check_source(path: &Path) -> Result<std::fs::Metadata, DecodeError> {
    let metadata = std::fs::metadata(path).map_err(|err| {
        DecodeError::NotFound(if err.kind() == ErrorKind::NotFound {
            format!("{} does not exist", path.display())
        } else {
            format!("cannot read {}: {err}", path.display())
        })
    })?;
    if !metadata.is_file() {
        return Err(DecodeError::NotFound(format!(
            "{} is not a file",
            path.display()
        )));
    }
    File::open(path)
        .map_err(|err| DecodeError::NotFound(format!("cannot read {}: {err}", path.display())))?;
    // Nothing can decode it, so ffmpeg is no help either.
    if metadata.len() == 0 {
        return Err(DecodeError::Unsupported("the file is empty".to_string()));
    }
    Ok(metadata)
}

fn route_for(path: &Path) -> Result<DecodeRoute, DecodeError> {
    formats::route_for_path(path).ok_or_else(|| {
        DecodeError::Unsupported(match formats::lowercase_extension(path) {
            Some(ext) if !ext.is_empty() => format!(".{ext} files are not supported"),
            _ => "files without an extension are not supported".to_string(),
        })
    })
}

fn needs_ffmpeg_for_extension(path: &Path) -> DecodeError {
    let ext = formats::format_extension(path).unwrap_or_default();
    DecodeError::NeedsFfmpeg(format!("{ext} files need ffmpeg"))
}

/// Named .wav/.wave, or a RIFF/WAVE file without a known extension.
fn is_wav(path: &Path) -> bool {
    matches!(
        formats::format_extension(path).as_deref(),
        Some("wav" | "wave")
    )
}

/// Asks ffmpeg whether it can read `path`. Ok(None) when ffmpeg did not
/// answer: decoding will tell.
fn probe_with_ffmpeg(ffmpeg: &Path, path: &Path) -> Result<Option<u64>, DecodeError> {
    match ffmpeg::probe_input(ffmpeg, path) {
        Some(InputInfo::Opened {
            duration_ms,
            has_audio: true,
        }) => Ok(duration_ms),
        Some(InputInfo::Opened {
            has_audio: false, ..
        }) => Err(DecodeError::Unsupported(
            "the file has no audio track".to_string(),
        )),
        Some(InputInfo::Unreadable(reason)) => Err(DecodeError::Unsupported(format!(
            "not a readable audio or video file (ffmpeg: {reason})"
        ))),
        None => Ok(None),
    }
}

/// Counts what the sink accepted, which decides whether a later failure is
/// an error or a truncation.
struct Sink<'a> {
    inner: &'a mut dyn FnMut(&[f32]) -> anyhow::Result<()>,
    samples: u64,
}

impl<'a> Sink<'a> {
    fn new(inner: &'a mut dyn FnMut(&[f32]) -> anyhow::Result<()>) -> Self {
        Self { inner, samples: 0 }
    }

    fn send(&mut self, block: &[f32]) -> anyhow::Result<()> {
        if block.is_empty() {
            return Ok(());
        }
        (self.inner)(block)?;
        self.samples += block.len() as u64;
        Ok(())
    }

    fn send_silence(&mut self, samples: u64) -> anyhow::Result<()> {
        let zeros = vec![0.0; usize::try_from(samples).map_or(WAV_BLOCK, |n| n.min(WAV_BLOCK))];
        let mut left = samples;
        while left > 0 {
            let n = usize::try_from(left).map_or(zeros.len(), |n| n.min(zeros.len()));
            self.send(&zeros[..n])?;
            left -= n as u64;
        }
        Ok(())
    }

    fn outcome(&self, decoder: DecoderKind, truncated: Option<String>) -> DecodeOutcome {
        DecodeOutcome {
            decoder,
            samples: self.samples,
            truncated,
        }
    }
}

/// Why a decoder stopped without an outcome.
enum Stop<E> {
    /// Cancelled, or the sink failed: final, no fallback.
    Halted(DecodeError),
    /// The decoder failed before any audio reached the sink.
    Unusable(E),
}

/// Some(..) when the file is 16 kHz mono 16-bit PCM and the fast path
/// decided the result; None to let symphonia (and ffmpeg) try instead.
fn decode_fast_wav(
    path: &Path,
    cancel: &AtomicBool,
    sink: &mut Sink<'_>,
) -> Option<Result<DecodeOutcome, DecodeError>> {
    let reader = hound::WavReader::open(path).ok()?;
    if !is_fast_wav(&reader.spec()) {
        return None;
    }
    let mut samples = reader.into_samples::<i16>();
    let mut block = Vec::with_capacity(WAV_BLOCK);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Some(Err(DecodeError::Cancelled));
        }
        block.clear();
        let mut failure = None;
        for sample in samples.by_ref().take(WAV_BLOCK) {
            match sample {
                // Same conversion as `read_wav_samples`.
                Ok(value) => block.push(value as f32 / i16::MAX as f32),
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
        }
        if let Err(err) = sink.send(&block) {
            return Some(Err(DecodeError::Failed(err)));
        }
        match failure {
            Some(err) if sink.samples == 0 => {
                debug!("WAV fast path failed on {}: {err}", path.display());
                return None;
            }
            Some(err) => {
                return Some(Ok(sink.outcome(
                    DecoderKind::Wav,
                    Some(format!("WAV data ends early: {err}")),
                )))
            }
            None if block.len() < WAV_BLOCK => break,
            None => {}
        }
    }
    // An empty data chunk is what a crashed recorder leaves behind; the other
    // decoders may still recover the audio.
    (sink.samples > 0).then(|| Ok(sink.outcome(DecoderKind::Wav, None)))
}

fn is_fast_wav(spec: &hound::WavSpec) -> bool {
    spec.sample_rate == TARGET_SAMPLE_RATE
        && spec.channels == 1
        && spec.bits_per_sample == 16
        && spec.sample_format == hound::SampleFormat::Int
}

fn fast_wav_duration_ms(path: &Path) -> Option<u64> {
    if !is_wav(path) {
        return None;
    }
    let reader = hound::WavReader::open(path).ok()?;
    let frames = u64::from(reader.duration());
    (is_fast_wav(&reader.spec()) && frames > 0)
        .then(|| frames * 1000 / u64::from(TARGET_SAMPLE_RATE))
}

fn decode_ffmpeg(
    ffmpeg: &Path,
    path: &Path,
    cancel: &AtomicBool,
    sink: &mut Sink<'_>,
) -> Result<DecodeOutcome, Stop<anyhow::Error>> {
    let mut sink_failed = false;
    let result = ffmpeg::decode_with_ffmpeg(ffmpeg, path, cancel, &mut |block| {
        sink.send(block).inspect_err(|_| sink_failed = true)
    });
    match result {
        Ok(_) => Ok(sink.outcome(DecoderKind::Ffmpeg, None)),
        Err(_) if cancel.load(Ordering::Relaxed) => Err(Stop::Halted(DecodeError::Cancelled)),
        Err(err) if sink_failed => Err(Stop::Halted(DecodeError::Failed(err))),
        Err(err) if sink.samples > 0 => {
            Ok(sink.outcome(DecoderKind::Ffmpeg, Some(format!("{err:#}"))))
        }
        Err(err) => Err(Stop::Unusable(err)),
    }
}

/// Why symphonia produced no audio for a file.
#[derive(Debug)]
enum Unusable {
    /// No container format recognised, or its header is damaged or cut short.
    Unrecognised(String),
    /// A container symphonia recognised but does not support in this form
    /// (an unusual WAV codec, compressed AIFC, a QuickTime file without
    /// `ftyp`, ...): ffmpeg may read it.
    Container(String),
    /// A container without any audio track.
    NoAudioTrack,
    /// Audio exists but no track has a usable decoder.
    Codec {
        name: Option<&'static str>,
        detail: String,
        duration_ms: Option<u64>,
    },
    /// Recognised and decodable, but failed before any audio came out.
    Corrupt(String),
    /// Read to the end without a failure, from a container that declares
    /// its audio track empty.
    Empty,
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unrecognised(detail) => write!(f, "unrecognised format ({detail})"),
            Self::Container(detail) => write!(f, "unsupported container ({detail})"),
            Self::NoAudioTrack => f.write_str("no audio track"),
            Self::Codec { name, detail, .. } => {
                write!(
                    f,
                    "{} audio cannot be decoded ({detail})",
                    name.unwrap_or("this")
                )
            }
            Self::Corrupt(detail) => f.write_str(detail),
            Self::Empty => f.write_str(NO_SAMPLES),
        }
    }
}

const NO_SAMPLES: &str = "the audio track holds no samples";

impl Unusable {
    /// The error when no ffmpeg is available to fall back to. NeedsFfmpeg
    /// only when the file shows that ffmpeg could help.
    fn into_error(self, path: &Path) -> DecodeError {
        match self {
            // Junk, or damage: unless the first bytes name a container only
            // ffmpeg reads (RF64 named .wav, say), ffmpeg would fail too.
            Self::Unrecognised(detail) => match formats::sniff(path) {
                Some(sniffed) if sniffed.route == DecodeRoute::Ffmpeg => {
                    DecodeError::NeedsFfmpeg(format!("{} files need ffmpeg", sniffed.ext))
                }
                _ => DecodeError::Unsupported(format!("not a readable audio file ({detail})")),
            },
            Self::Container(detail) => {
                let ext = formats::format_extension(path).unwrap_or_default();
                DecodeError::NeedsFfmpeg(format!(
                    "this .{ext} file can only be read with ffmpeg ({detail})"
                ))
            }
            Self::NoAudioTrack => {
                DecodeError::Unsupported("the file has no audio track".to_string())
            }
            Self::Codec { name, .. } => DecodeError::NeedsFfmpeg(match name {
                Some(name) => format!("{name} audio needs ffmpeg"),
                None => "this audio codec needs ffmpeg".to_string(),
            }),
            Self::Corrupt(detail) => DecodeError::Failed(anyhow::anyhow!(detail)),
            Self::Empty => DecodeError::Failed(anyhow::anyhow!(NO_SAMPLES)),
        }
    }

    /// The error when the ffmpeg fallback failed too, before any audio.
    fn with_ffmpeg_error(self, err: anyhow::Error) -> DecodeError {
        match self {
            Self::Unrecognised(detail) | Self::Container(detail) => DecodeError::Unsupported(
                format!("not a readable audio file (symphonia: {detail}; {err:#})"),
            ),
            Self::NoAudioTrack => {
                DecodeError::Unsupported(format!("the file has no audio track ({err:#})"))
            }
            unusable @ (Self::Codec { .. } | Self::Corrupt(_) | Self::Empty) => {
                DecodeError::Failed(err.context(unusable.to_string()))
            }
        }
    }
}

fn decode_symphonia(
    path: &Path,
    cancel: &AtomicBool,
    sink: &mut Sink<'_>,
) -> Result<DecodeOutcome, Stop<Unusable>> {
    let mut input = SymphoniaInput::open(path).map_err(Stop::Unusable)?;
    let mut mono = Vec::new();
    let mut to_16k = To16k::default();

    let failure: Option<Unusable> = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Halted(DecodeError::Cancelled));
        }
        let block = match guarded(|| input.next_block(&mut mono)) {
            Ok(Ok(Some(block))) => block,
            Ok(Ok(None)) => break None,
            Ok(Err(unusable)) => break Some(unusable),
            // The reader's state is unknown after a panic: stop using it.
            Err(panic) => break Some(Unusable::Corrupt(panic)),
        };
        let silence = input.timeline.silence_before(block.ts, to_16k.position());
        if silence > 0 {
            // The resampler's audio comes first, then the missing stretch.
            match to_16k.finish() {
                Ok(held) => sink.send(held).map_err(halt)?,
                Err(err) => break Some(Unusable::Corrupt(format!("{err:#}"))),
            }
            sink.send_silence(silence).map_err(halt)?;
            to_16k.skip(silence);
        }
        match to_16k.push(block.rate, &mono) {
            Ok(block) => sink.send(block).map_err(halt)?,
            Err(err) => break Some(Unusable::Corrupt(format!("{err:#}"))),
        }
    };

    let failure = match failure {
        // Nothing reached the sink: drop what the resampler holds so that
        // another decoder can start from scratch.
        Some(unusable) if sink.samples == 0 => return Err(Stop::Unusable(unusable)),
        failure => failure,
    };
    // Keep the audio still in the resampler, also after a mid-stream failure.
    let mut truncated = failure.map(|unusable| unusable.to_string());
    match to_16k.finish() {
        Ok(block) => sink.send(block).map_err(halt)?,
        Err(err) => {
            truncated.get_or_insert_with(|| format!("{err:#}"));
        }
    }
    if sink.samples == 0 {
        return Err(Stop::Unusable(match truncated {
            Some(reason) => Unusable::Corrupt(reason),
            None if input.params.n_frames == Some(0) => Unusable::Empty,
            None => Unusable::Corrupt(NO_SAMPLES.to_string()),
        }));
    }
    if let Some(reason) = &truncated {
        debug!(
            "Decoding {} stopped early after {} samples: {reason}",
            path.display(),
            sink.samples
        );
    }
    Ok(sink.outcome(DecoderKind::Symphonia, truncated))
}

/// Resamples decoded blocks to 16 kHz, flushing and rebuilding the resampler
/// whenever the rate changes. The rate is that of each decoded block, never
/// the track's: HE-AAC decodes at half the rate its container declares, and
/// some streams change rate midway.
#[derive(Default)]
struct To16k {
    resampler: Option<StreamResampler>,
    out: Vec<f32>,
    /// Output of the resamplers already finished, and silence skipped.
    finished: u64,
}

impl To16k {
    /// Where the audio pushed next lands in the 16 kHz output.
    fn position(&self) -> u64 {
        self.finished
            + self
                .resampler
                .as_ref()
                .map_or(0, StreamResampler::output_len)
    }

    /// Returns the 16 kHz audio that became available.
    fn push(&mut self, rate: u32, mono: &[f32]) -> anyhow::Result<&[f32]> {
        self.out.clear();
        if self.resampler.as_ref().map(StreamResampler::in_hz) != Some(rate) {
            self.finish_resampler()?;
            self.resampler = Some(StreamResampler::new(rate, TARGET_SAMPLE_RATE)?);
        }
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.push(mono, &mut self.out)?;
        }
        Ok(&self.out)
    }

    /// Returns the audio still held by the resampler.
    fn finish(&mut self) -> anyhow::Result<&[f32]> {
        self.out.clear();
        self.finish_resampler()?;
        Ok(&self.out)
    }

    /// Moves the output position past silence the caller sent after `finish`.
    fn skip(&mut self, samples: u64) {
        self.finished += samples;
    }

    fn finish_resampler(&mut self) -> anyhow::Result<()> {
        if let Some(mut resampler) = self.resampler.take() {
            self.finished += resampler.output_len();
            resampler.finish(&mut self.out)?;
        }
        Ok(())
    }
}

fn halt<E>(sink_error: anyhow::Error) -> Stop<E> {
    Stop::Halted(DecodeError::Failed(sink_error))
}

/// Runs symphonia code, turning a panic on malformed input into an error.
fn guarded<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        format!("decoder crashed: {message}")
    })
}

/// An opened file with the audio track to decode and its decoder.
struct SymphoniaInput {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    params: CodecParameters,
    sample_buf: Option<SampleBuffer<f32>>,
    bad_packets: u32,
    timeline: Timeline,
}

/// A decoded packet, downmixed into the caller's buffer.
struct Block {
    rate: u32,
    /// The packet's timestamp, in the track's time base.
    ts: u64,
}

impl SymphoniaInput {
    fn open(path: &Path) -> Result<Self, Unusable> {
        let file = File::open(path).map_err(|err| Unusable::Corrupt(err.to_string()))?;
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = formats::lowercase_extension(path) {
            hint.with_extension(&ext);
        }
        let format_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        let format = guarded(|| {
            symphonia::default::get_probe().format(
                &hint,
                stream,
                &format_opts,
                &MetadataOptions::default(),
            )
        })
        .map_err(Unusable::Corrupt)?
        .map_err(probe_failure)?
        .format;

        let (track_id, params, decoder) = select_track(format.as_ref())?;
        let timeline = Timeline::new(&params, mp4_lead(path, track_id));
        Ok(Self {
            format,
            decoder,
            track_id,
            params,
            sample_buf: None,
            bad_packets: 0,
            timeline,
        })
    }

    /// The track's length, plus the silence it starts with.
    fn duration_ms(&self) -> Option<u64> {
        let lead_ms = self.timeline.lead * 1000 / u64::from(TARGET_SAMPLE_RATE);
        duration_ms(&self.params).map(|ms| ms + lead_ms)
    }

    /// Decodes up to the next packet of the selected track that holds audio,
    /// downmixed into `mono`. None at the end.
    fn next_block(&mut self, mono: &mut Vec<f32>) -> Result<Option<Block>, Unusable> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(err)) if err.kind() == ErrorKind::UnexpectedEof => {
                    return Ok(None)
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.reset()?;
                    continue;
                }
                Err(err) => {
                    return Err(Unusable::Corrupt(format!("reading the file failed: {err}")))
                }
            };
            // Metadata revisions (chained Ogg streams) would otherwise pile up.
            while !self.format.metadata().is_latest() {
                self.format.metadata().pop();
            }
            if packet.track_id() != self.track_id {
                continue;
            }

            let err = match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let frames = decoded.frames();
                    let channels = spec.channels.count();
                    if frames == 0 {
                        // Priming packets (Vorbis) carry no audio.
                        continue;
                    }
                    if channels == 0 || spec.rate == 0 {
                        SymphoniaError::DecodeError("decoded audio has no channels or rate")
                    } else {
                        self.bad_packets = 0;
                        let needed = frames * channels;
                        let buf = match &mut self.sample_buf {
                            Some(buf) if buf.capacity() >= needed => buf,
                            slot => slot.insert(SampleBuffer::new(
                                decoded.capacity().max(frames) as u64,
                                spec,
                            )),
                        };
                        buf.copy_interleaved_ref(decoded);
                        downmix(buf.samples(), channels, mono);
                        return Ok(Some(Block {
                            rate: spec.rate,
                            ts: packet.ts(),
                        }));
                    }
                }
                Err(err) => err,
            };

            match err {
                // A corrupt packet: skip it, unless the stream is hopeless.
                SymphoniaError::DecodeError(_) | SymphoniaError::IoError(_) => {
                    self.bad_packets += 1;
                    if self.bad_packets >= MAX_BAD_PACKETS {
                        return Err(Unusable::Corrupt(format!(
                            "{MAX_BAD_PACKETS} packets in a row could not be decoded ({err})"
                        )));
                    }
                }
                SymphoniaError::ResetRequired => self.reset()?,
                SymphoniaError::Unsupported(detail) => {
                    return Err(Unusable::Codec {
                        name: codec_name(self.params.codec),
                        detail: detail.to_string(),
                        duration_ms: duration_ms(&self.params),
                    })
                }
                err => return Err(Unusable::Corrupt(format!("decoding failed: {err}"))),
            }
        }
    }

    /// Re-examines the tracks and recreates the decoder, as symphonia asks
    /// after `ResetRequired`.
    fn reset(&mut self) -> Result<(), Unusable> {
        let (track_id, params, decoder) = select_track(self.format.as_ref())?;
        // A new stream (chained Ogg) starts a new clock where the last ended.
        self.timeline = Timeline::new(&params, 0);
        self.track_id = track_id;
        self.params = params;
        self.decoder = decoder;
        Ok(())
    }
}

/// Sorts out why symphonia could not open a file.
fn probe_failure(err: SymphoniaError) -> Unusable {
    match &err {
        // A reader took the file but lacks a feature it uses. Not so for
        // the probe finding no reader at all, nor for an mp4 that lost its
        // index (an interrupted recording), which ffmpeg cannot read either.
        SymphoniaError::Unsupported(detail)
            if !detail.starts_with("core (probe)") && !detail.contains("missing moov") =>
        {
            Unusable::Container(err.to_string())
        }
        _ => Unusable::Unrecognised(err.to_string()),
    }
}

/// Places decoded packets in the 16 kHz output by their timestamps, so that
/// stretches the file does not hold (a delayed start, lost packets) can be
/// filled with silence, as the ffmpeg route does. Audio is never dropped:
/// a packet that comes early is kept where the audio before it ends.
struct Timeline {
    /// Seconds per timestamp tick, as (numerator, denominator).
    time_base: Option<(u64, u64)>,
    /// Output position of the first packet: where an mp4 track starts.
    lead: u64,
    /// A timestamp and the output position it maps to.
    anchor: Option<(u64, u64)>,
}

impl Timeline {
    fn new(params: &CodecParameters, lead: u64) -> Self {
        let time_base = match (params.time_base, params.sample_rate) {
            (Some(base), _) => Some((u64::from(base.numer), u64::from(base.denom))),
            (None, Some(rate)) => Some((1, u64::from(rate))),
            (None, None) => None,
        };
        Self {
            time_base: time_base.filter(|&(numer, denom)| numer > 0 && denom > 0),
            lead,
            anchor: None,
        }
    }

    /// Samples of silence that belong before a packet with timestamp `ts`,
    /// when the audio so far ends at `position`.
    fn silence_before(&mut self, ts: u64, position: u64) -> u64 {
        let lead = self.lead;
        let (anchor_ts, anchor_position) = *self
            .anchor
            .get_or_insert_with(|| (ts, position.saturating_add(lead)));
        let gap = ts
            .checked_sub(anchor_ts)
            .and_then(|ticks| self.to_samples(ticks))
            .map(|offset| {
                anchor_position
                    .saturating_add(offset)
                    .saturating_sub(position)
            });
        match gap {
            Some(gap) if gap <= MAX_IGNORED_GAP => 0,
            Some(gap) if gap <= MAX_FILLED_GAP => gap,
            // Earlier than the anchor, implausibly late or no time base: the
            // timestamps restarted or cannot be trusted. Measure from here.
            _ => {
                self.anchor = Some((ts, position));
                0
            }
        }
    }

    fn to_samples(&self, ticks: u64) -> Option<u64> {
        let (numer, denom) = self.time_base?;
        let samples = u128::from(ticks) * u128::from(numer) * u128::from(TARGET_SAMPLE_RATE)
            / u128::from(denom);
        u64::try_from(samples).ok()
    }
}

/// An mp4 atom: its type and where its body lies in the file.
struct Atom {
    kind: [u8; 4],
    body: u64,
    end: u64,
}

/// Output position, in 16 kHz samples, at which an mp4/mov track starts:
/// how much its leading empty edits exceed the earliest track's. symphonia
/// 0.5 ignores edit lists, so audio recorded to start after the video would
/// otherwise move to the start. 0 for other files, and when in doubt.
fn mp4_lead(path: &Path, track_id: u32) -> u64 {
    let is_mp4 = matches!(
        formats::format_extension(path).as_deref(),
        Some("mp4" | "m4a" | "m4b" | "m4v" | "mov" | "3gp")
    );
    let lead = is_mp4
        .then(|| mp4_leads(path))
        .flatten()
        .and_then(|(timescale, leads)| {
            let earliest = *leads.iter().min()?;
            let ticks = leads.get(usize::try_from(track_id).ok()?)? - earliest;
            u64::try_from(
                u128::from(ticks) * u128::from(TARGET_SAMPLE_RATE) / u128::from(timescale),
            )
            .ok()
        });
    lead.unwrap_or(0)
}

/// The movie timescale, and the leading empty-edit time of each track in
/// file order (symphonia's track ids), in movie ticks.
fn mp4_leads(path: &Path) -> Option<(u64, Vec<u64>)> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let find = |file: &mut File, start: u64, end: u64, kind: &[u8; 4]| {
        mp4_atoms(file, start, end)
            .ok()?
            .into_iter()
            .find(|atom| &atom.kind == kind)
    };
    let moov = find(&mut file, 0, len, b"moov")?;
    let mut timescale = None;
    let mut leads = Vec::new();
    for atom in mp4_atoms(&mut file, moov.body, moov.end).ok()? {
        match &atom.kind {
            b"mvhd" => {
                let body = mp4_body(&mut file, &atom)?;
                let at = if *body.first()? == 1 { 20 } else { 12 };
                timescale = Some(u64::from(u32::from_be_bytes(
                    body.get(at..at + 4)?.try_into().ok()?,
                )));
            }
            b"trak" => {
                let elst = find(&mut file, atom.body, atom.end, b"edts")
                    .and_then(|edts| find(&mut file, edts.body, edts.end, b"elst"));
                let lead = match elst {
                    Some(elst) => empty_edits(&mp4_body(&mut file, &elst)?)?,
                    None => 0,
                };
                leads.push(lead);
            }
            _ => {}
        }
    }
    Some((timescale.filter(|&ticks| ticks > 0)?, leads))
}

/// Total duration of the empty edits (media time −1) an `elst` body starts with.
fn empty_edits(elst: &[u8]) -> Option<u64> {
    let version = *elst.first()?;
    let count = u32::from_be_bytes(elst.get(4..8)?.try_into().ok()?);
    let entry_len = if version == 1 { 20 } else { 12 };
    let mut lead = 0u64;
    for entry in elst
        .get(8..)?
        .chunks_exact(entry_len)
        .take(usize::try_from(count).ok()?)
    {
        let (duration, media_time) = if version == 1 {
            (
                u64::from_be_bytes(entry[..8].try_into().ok()?),
                i64::from_be_bytes(entry[8..16].try_into().ok()?),
            )
        } else {
            (
                u64::from(u32::from_be_bytes(entry[..4].try_into().ok()?)),
                i64::from(i32::from_be_bytes(entry[4..8].try_into().ok()?)),
            )
        };
        if media_time != -1 {
            break;
        }
        lead = lead.saturating_add(duration);
    }
    Some(lead)
}

/// The atoms in `start..end`, up to the first one that does not fit.
fn mp4_atoms(file: &mut File, mut pos: u64, end: u64) -> std::io::Result<Vec<Atom>> {
    let mut atoms = Vec::new();
    while end.saturating_sub(pos) >= 8 {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8])?;
        let kind = [header[4], header[5], header[6], header[7]];
        let (size, header_len) =
            match u32::from_be_bytes([header[0], header[1], header[2], header[3]]) {
                0 => (end - pos, 8),
                1 => {
                    file.read_exact(&mut header[8..])?;
                    let mut large = [0u8; 8];
                    large.copy_from_slice(&header[8..]);
                    (u64::from_be_bytes(large), 16)
                }
                size => (u64::from(size), 8),
            };
        if size < header_len || size > end - pos {
            break;
        }
        atoms.push(Atom {
            kind,
            body: pos + header_len,
            end: pos + size,
        });
        pos += size;
    }
    Ok(atoms)
}

/// The body of a small atom (at most 1 MiB of it).
fn mp4_body(file: &mut File, atom: &Atom) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(atom.body)).ok()?;
    let mut body = Vec::new();
    file.take((atom.end - atom.body).min(1 << 20))
        .read_to_end(&mut body)
        .ok()?;
    Some(body)
}

/// The default track if it can be decoded, else the first track that can.
/// `default_track()` is just the first track, which in mp4/mov is often video.
fn select_track(
    format: &dyn FormatReader,
) -> Result<(u32, CodecParameters, Box<dyn Decoder>), Unusable> {
    let codecs = symphonia::default::get_codecs();
    let decodable = |track: &&Track| {
        track.codec_params.codec != sc::CODEC_TYPE_NULL
            && codecs.get_codec(track.codec_params.codec).is_some()
    };
    let default_id = format.default_track().map(|track| track.id);
    let candidates = format.default_track().filter(decodable).into_iter().chain(
        format
            .tracks()
            .iter()
            .filter(|track| Some(track.id) != default_id)
            .filter(decodable),
    );

    let mut first_failure: Option<(&Track, String)> = None;
    for track in candidates {
        let made = guarded(|| codecs.make(&track.codec_params, &DecoderOptions::default()));
        match made {
            Ok(Ok(decoder)) => return Ok((track.id, track.codec_params.clone(), decoder)),
            Ok(Err(err)) if first_failure.is_none() => {
                first_failure = Some((track, err.to_string()));
            }
            Err(panic) if first_failure.is_none() => first_failure = Some((track, panic)),
            Ok(Err(_)) | Err(_) => {}
        }
    }

    // Audio tracks have a codec or at least a sample rate; video has neither.
    let audio_track = first_failure.as_ref().map(|(track, _)| *track).or_else(|| {
        format.tracks().iter().find(|track| {
            track.codec_params.codec != sc::CODEC_TYPE_NULL
                || track.codec_params.sample_rate.is_some()
        })
    });
    match audio_track {
        Some(track) => Err(Unusable::Codec {
            name: codec_name(track.codec_params.codec),
            detail: first_failure
                .map(|(_, detail)| detail)
                .unwrap_or_else(|| "no decoder for this codec".to_string()),
            duration_ms: duration_ms(&track.codec_params),
        }),
        None => Err(Unusable::NoAudioTrack),
    }
}

/// Track length from the container. `n_frames` counts in `time_base` ticks
/// (mp4: the media timescale), which stays right when the decoded rate
/// differs from the container rate (HE-AAC).
fn duration_ms(params: &CodecParameters) -> Option<u64> {
    let frames = u128::from(params.n_frames?);
    let ms = match params.time_base {
        Some(base) if base.numer > 0 && base.denom > 0 => {
            frames * u128::from(base.numer) * 1000 / u128::from(base.denom)
        }
        _ => frames * 1000 / u128::from(params.sample_rate.filter(|&rate| rate > 0)?),
    };
    u64::try_from(ms).ok().filter(|&ms| ms > 0)
}

/// For messages about codecs symphonia cannot decode here.
fn codec_name(codec: CodecType) -> Option<&'static str> {
    Some(match codec {
        sc::CODEC_TYPE_AAC => "AAC",
        sc::CODEC_TYPE_MP1 | sc::CODEC_TYPE_MP2 | sc::CODEC_TYPE_MP3 => "MPEG audio",
        sc::CODEC_TYPE_FLAC => "FLAC",
        sc::CODEC_TYPE_VORBIS => "Vorbis",
        sc::CODEC_TYPE_OPUS => "Opus",
        sc::CODEC_TYPE_ALAC => "ALAC",
        sc::CODEC_TYPE_ADPCM_G722
        | sc::CODEC_TYPE_ADPCM_G726
        | sc::CODEC_TYPE_ADPCM_G726LE
        | sc::CODEC_TYPE_ADPCM_MS
        | sc::CODEC_TYPE_ADPCM_IMA_WAV
        | sc::CODEC_TYPE_ADPCM_IMA_QT => "ADPCM",
        sc::CODEC_TYPE_SPEEX => "Speex",
        sc::CODEC_TYPE_WMA => "WMA",
        sc::CODEC_TYPE_EAC3 => "E-AC-3",
        sc::CODEC_TYPE_AC4 => "AC-4",
        sc::CODEC_TYPE_DCA => "DTS",
        sc::CODEC_TYPE_WAVPACK => "WavPack",
        sc::CODEC_TYPE_MONKEYS_AUDIO => "Monkey's Audio",
        sc::CODEC_TYPE_TTA => "TTA",
        sc::CODEC_TYPE_MUSEPACK => "Musepack",
        sc::CODEC_TYPE_ATRAC1
        | sc::CODEC_TYPE_ATRAC3
        | sc::CODEC_TYPE_ATRAC3PLUS
        | sc::CODEC_TYPE_ATRAC9 => "ATRAC",
        _ => return None,
    })
}

/// Interleaved frames → mono by plain average (as the recorder does), with
/// non-finite samples replaced by 0: float files can contain NaN/inf, which
/// would poison the resampler and the VAD.
fn downmix(interleaved: &[f32], channels: usize, mono: &mut Vec<f32>) {
    let finite = |sample: f32| if sample.is_finite() { sample } else { 0.0 };
    mono.clear();
    if channels == 1 {
        mono.extend(interleaved.iter().map(|&sample| finite(sample)));
    } else {
        let scale = 1.0 / channels as f32;
        mono.extend(
            interleaved.chunks_exact(channels).map(|frame| {
                finite(frame.iter().map(|&sample| finite(sample)).sum::<f32>() * scale)
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_toolkit::read_wav_samples;
    use crate::file_transcription::ffmpeg::tests::{ffmpeg_or_skip, make_fixture};
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::f64::consts::PI;

    const NO_FFMPEG: &str = "/nonexistent/ffmpeg";

    struct Decoded {
        samples: Vec<f32>,
        outcome: DecodeOutcome,
        largest_block: usize,
    }

    fn decode_with(path: &Path, opts: &DecodeOptions) -> Result<Decoded, DecodeError> {
        let mut samples = Vec::new();
        let mut largest_block = 0;
        let outcome = decode_to_16k_mono(path, opts, &AtomicBool::new(false), &mut |block| {
            largest_block = largest_block.max(block.len());
            samples.extend_from_slice(block);
            Ok(())
        })?;
        assert_eq!(outcome.samples, samples.len() as u64);
        Ok(Decoded {
            samples,
            outcome,
            largest_block,
        })
    }

    fn decode(path: &Path) -> Decoded {
        decode_with(path, &DecodeOptions::default())
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    fn no_ffmpeg() -> DecodeOptions {
        DecodeOptions {
            ffmpeg: Some(PathBuf::from(NO_FFMPEG)),
        }
    }

    fn expected_len(frames: usize, rate: u32) -> usize {
        (frames as f64 * 16_000.0 / f64::from(rate)).round() as usize
    }

    fn assert_len_within(got: usize, want: usize, tolerance: usize, what: &str) {
        assert!(
            got.abs_diff(want) <= tolerance,
            "{what}: {got} samples, want {want} ± {tolerance}"
        );
    }

    fn tone(rate: u32, freq: f64, i: usize) -> f64 {
        (2.0 * PI * freq * i as f64 / f64::from(rate)).sin()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// Without the first and last 0.5 s, where the resampler rings on the
    /// tone's abrupt start and end.
    fn middle(samples: &[f32]) -> &[f32] {
        &samples[8_000..samples.len() - 8_000]
    }

    fn write_wav_i16(path: &Path, rate: u32, channels: u16, frames: usize) {
        let spec = WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            let value = (tone(rate, 440.0, i) * 12_000.0) as i16;
            for _ in 0..channels {
                writer.write_sample(value).unwrap();
            }
        }
        writer.finalize().unwrap();
    }

    /// A big-endian AIFF (FORM/AIFF with COMM and SSND), which hound cannot write.
    fn write_aiff(path: &Path, rate: u32, channels: u16, frames: &[Vec<i16>]) {
        let data_len = (frames.len() * channels as usize * 2) as u32;
        let mut comm = Vec::new();
        comm.extend_from_slice(&(channels as i16).to_be_bytes());
        comm.extend_from_slice(&(frames.len() as u32).to_be_bytes());
        comm.extend_from_slice(&16i16.to_be_bytes());
        comm.extend_from_slice(&extended_be(rate));

        let mut body = Vec::new();
        body.extend_from_slice(b"AIFF");
        body.extend_from_slice(b"COMM");
        body.extend_from_slice(&(comm.len() as u32).to_be_bytes());
        body.extend_from_slice(&comm);
        body.extend_from_slice(b"SSND");
        body.extend_from_slice(&(data_len + 8).to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // offset
        body.extend_from_slice(&0u32.to_be_bytes()); // block size
        for frame in frames {
            for &sample in frame {
                body.extend_from_slice(&sample.to_be_bytes());
            }
        }

        let mut file = Vec::new();
        file.extend_from_slice(b"FORM");
        file.extend_from_slice(&(body.len() as u32).to_be_bytes());
        file.extend_from_slice(&body);
        std::fs::write(path, file).unwrap();
    }

    /// An integer sample rate as an 80-bit IEEE 754 extended float.
    fn extended_be(rate: u32) -> [u8; 10] {
        let exponent = 31 - rate.leading_zeros();
        let mantissa = u64::from(rate) << (63 - exponent);
        let mut out = [0u8; 10];
        out[..2].copy_from_slice(&((16_383 + exponent) as u16).to_be_bytes());
        out[2..].copy_from_slice(&mantissa.to_be_bytes());
        out
    }

    #[test]
    fn fast_path_matches_read_wav_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("speech.wav");
        write_wav_i16(&path, 16_000, 1, 16_000 * 9 + 123);

        let decoded = decode(&path);
        assert_eq!(decoded.outcome.decoder, DecoderKind::Wav);
        assert_eq!(decoded.outcome.truncated, None);
        assert_eq!(decoded.samples, read_wav_samples(&path).unwrap());
        assert!(decoded.largest_block <= WAV_BLOCK);

        let probe = probe(&path, &DecodeOptions::default()).unwrap();
        assert_eq!(probe.route, DecodeRoute::Symphonia);
        assert_eq!(probe.duration_ms, Some(9_007));
        assert!(probe.source_size_bytes > 16_000 * 9 * 2);
        assert!(probe.source_modified_at.is_some());
    }

    #[test]
    fn stereo_44k_wav_decodes_to_the_exact_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("music.WAV");
        let frames = 44_100 * 7 + 31;
        write_wav_i16(&path, 44_100, 2, frames);

        let decoded = decode(&path);
        assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia);
        assert_eq!(decoded.outcome.truncated, None);
        assert_len_within(
            decoded.samples.len(),
            expected_len(frames, 44_100),
            1,
            "44.1k",
        );
        // The 440 Hz tone at 12000/32768 survives downmix and resampling.
        let level = peak(middle(&decoded.samples));
        assert!((level - 0.366).abs() < 0.02, "peak {level}");

        let probe = probe(&path, &no_ffmpeg()).unwrap();
        assert_len_within(probe.duration_ms.unwrap() as usize, 7_000, 20, "probe");
    }

    #[test]
    fn opposite_channels_cancel_in_the_downmix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phase.wav");
        let spec = WavSpec {
            channels: 2,
            sample_rate: 22_050,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for i in 0..22_050 * 3 {
            let value = (tone(22_050, 1_000.0, i) * 20_000.0) as i16;
            writer.write_sample(value).unwrap();
            writer.write_sample(-value).unwrap();
        }
        writer.finalize().unwrap();

        let decoded = decode(&path);
        assert_len_within(decoded.samples.len(), 48_000, 1, "22.05k");
        assert!(peak(&decoded.samples) < 1e-4, "{}", peak(&decoded.samples));
    }

    #[test]
    fn non_finite_float_samples_become_silence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("float.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        for i in 0..48_000 * 2 {
            let value = match i % 9_000 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                _ => (tone(48_000, 300.0, i) * 0.5) as f32,
            };
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();

        let decoded = decode(&path);
        assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia);
        assert_len_within(decoded.samples.len(), 32_000, 1, "48k float");
        assert!(decoded.samples.iter().all(|s| s.is_finite()));
        assert!(peak(&decoded.samples) > 0.4);
    }

    #[test]
    fn eight_khz_24_bit_wav_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phone.wav");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 24,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(&path, spec).unwrap();
        let frames = 8_000 * 4 + 3;
        for i in 0..frames {
            writer
                .write_sample((tone(8_000, 500.0, i) * 4_000_000.0) as i32)
                .unwrap();
        }
        writer.finalize().unwrap();

        let decoded = decode(&path);
        assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia);
        assert_len_within(decoded.samples.len(), expected_len(frames, 8_000), 1, "8k");
        let level = peak(middle(&decoded.samples));
        assert!((level - 0.477).abs() < 0.02, "peak {level}");
    }

    #[test]
    fn truncated_wav_keeps_the_audio_before_the_cut() {
        let dir = tempfile::tempdir().unwrap();
        for (name, rate) in [("fast.wav", 16_000), ("slow.wav", 44_100)] {
            let path = dir.path().join(name);
            write_wav_i16(&path, rate, 1, rate as usize * 6);
            let bytes = std::fs::read(&path).unwrap();
            std::fs::write(&path, &bytes[..bytes.len() / 2 + 1]).unwrap();

            let decoded = decode_with(&path, &no_ffmpeg()).unwrap();
            let full = 16_000 * 6;
            assert!(
                decoded.outcome.truncated.is_some() || decoded.samples.len() < full,
                "{name}: {:?}",
                decoded.outcome
            );
            assert_len_within(decoded.samples.len(), full / 2, 16_000 / 10, name);
        }
    }

    #[test]
    fn junk_named_wav_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.wav");
        std::fs::write(&path, "this is not audio, just some text. ".repeat(500)).unwrap();

        // Nothing suggests that ffmpeg could read it.
        let err = decode_with(&path, &no_ffmpeg()).err().unwrap();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
        let err = probe(&path, &no_ffmpeg()).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");

        if ffmpeg_or_skip("junk_named_wav_is_rejected (with ffmpeg)").is_some() {
            let err = decode_with(&path, &DecodeOptions::default()).err().unwrap();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
            let err = probe(&path, &DecodeOptions::default()).unwrap_err();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
        }
    }

    #[test]
    fn empty_files_are_unsupported_whatever_the_decoder() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["memo.wav", "call.m4a", "voice.opus", "noext"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"").unwrap();
            for opts in [no_ffmpeg(), DecodeOptions::default()] {
                let err = probe(&path, &opts).unwrap_err();
                assert!(
                    matches!(&err, DecodeError::Unsupported(m) if m == "the file is empty"),
                    "{name}: {err}"
                );
                let err = decode_with(&path, &opts).err().unwrap();
                assert!(
                    matches!(&err, DecodeError::Unsupported(m) if m == "the file is empty"),
                    "{name}: {err}"
                );
            }
        }
    }

    #[test]
    fn header_only_wavs_decode_to_no_samples() {
        let dir = tempfile::tempdir().unwrap();
        for (name, rate, channels) in [
            ("memo.wav", 16_000, 1),
            ("music.wav", 44_100, 2),
            ("rec.tmp", 16_000, 1),
        ] {
            let path = dir.path().join(name);
            write_wav_i16(&path, rate, channels, 0);
            for opts in [no_ffmpeg(), DecodeOptions::default()] {
                let decoded =
                    decode_with(&path, &opts).unwrap_or_else(|err| panic!("{name}: {err}"));
                assert!(decoded.samples.is_empty(), "{name}");
                assert_eq!(decoded.outcome.truncated, None, "{name}");
                assert!(probe(&path, &opts).is_ok(), "{name}");
            }
        }
    }

    #[test]
    fn wavs_without_a_known_extension_are_read_by_content() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["noext", "rec.tmp", "tmp.Ab12Cd"] {
            let path = dir.path().join(name);
            write_wav_i16(&path, 16_000, 1, 16_000 * 3 + 7);
            let decoded = decode_with(&path, &no_ffmpeg()).unwrap();
            assert_eq!(decoded.outcome.decoder, DecoderKind::Wav, "{name}");
            assert_eq!(decoded.samples, read_wav_samples(&path).unwrap(), "{name}");
            let probe = probe(&path, &no_ffmpeg()).unwrap();
            assert_eq!(probe.route, DecodeRoute::Symphonia);
            assert_eq!(probe.duration_ms, Some(3_000));
        }

        let path = dir.path().join("take");
        write_wav_i16(&path, 44_100, 2, 44_100 * 2);
        let decoded = decode_with(&path, &no_ffmpeg()).unwrap();
        assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia);
        assert_len_within(decoded.samples.len(), 32_000, 1, "44.1k without extension");
    }

    #[test]
    fn containers_symphonia_lacks_need_ffmpeg() {
        let dir = tempfile::tempdir().unwrap();
        // GSM 6.10 in WAV: symphonia knows WAV but not this codec.
        let gsm = dir.path().join("dictation.wav");
        let mut fmt = Vec::new();
        for field in [0x31u16, 1] {
            fmt.extend_from_slice(&field.to_le_bytes());
        }
        fmt.extend_from_slice(&8_000u32.to_le_bytes());
        fmt.extend_from_slice(&1_625u32.to_le_bytes());
        for field in [65u16, 0, 2, 320] {
            fmt.extend_from_slice(&field.to_le_bytes());
        }
        let data = vec![0u8; 650];
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        wav.extend_from_slice(&fmt);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(&gsm, wav).unwrap();

        // RF64 (a WAV over 4 GB) named .wav: symphonia reads no RF64.
        let rf64 = dir.path().join("broadcast.wav");
        let mut bytes = b"RF64\xff\xff\xff\xffWAVEds64".to_vec();
        bytes.extend_from_slice(&28u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 28]);
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        for field in [1u16, 1] {
            bytes.extend_from_slice(&field.to_le_bytes());
        }
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&32_000u32.to_le_bytes());
        for field in [2u16, 16] {
            bytes.extend_from_slice(&field.to_le_bytes());
        }
        bytes.extend_from_slice(b"data\xff\xff\xff\xff");
        bytes.extend_from_slice(&[0u8; 3_200]);
        std::fs::write(&rf64, bytes).unwrap();

        for path in [&gsm, &rf64] {
            let err = probe(path, &no_ffmpeg()).unwrap_err();
            assert!(matches!(err, DecodeError::NeedsFfmpeg(_)), "{err}");
            let err = decode_with(path, &no_ffmpeg()).err().unwrap();
            assert!(matches!(err, DecodeError::NeedsFfmpeg(_)), "{err}");
        }
    }

    #[test]
    fn ffmpeg_formats_without_ffmpeg_need_ffmpeg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.OPUS");
        std::fs::write(&path, b"OggS fake opus").unwrap();

        let err = probe(&path, &no_ffmpeg()).unwrap_err();
        assert!(matches!(&err, DecodeError::NeedsFfmpeg(m) if m == "opus files need ffmpeg"));
        assert_eq!(
            err.to_string(),
            "opus files need ffmpeg, but ffmpeg was not found"
        );
        let err = decode_with(&path, &no_ffmpeg()).err().unwrap();
        assert!(matches!(err, DecodeError::NeedsFfmpeg(_)), "{err}");
    }

    #[test]
    fn missing_files_and_folders_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        for path in [dir.path().join("missing.mp3"), dir.path().to_path_buf()] {
            let err = probe(&path, &no_ffmpeg()).unwrap_err();
            assert!(matches!(err, DecodeError::NotFound(_)), "{err}");
            let err = decode_with(&path, &no_ffmpeg()).err().unwrap();
            assert!(matches!(err, DecodeError::NotFound(_)), "{err}");
        }
    }

    #[test]
    fn unknown_extensions_are_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["notes.xyz", "README"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"data").unwrap();
            let err = probe(&path, &no_ffmpeg()).unwrap_err();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
            let err = decode_with(&path, &no_ffmpeg()).err().unwrap();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
        }
    }

    #[test]
    fn aiff_goes_through_symphonia() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.aiff");
        let frames: Vec<Vec<i16>> = (0..22_050 * 3 + 11)
            .map(|i| {
                let value = (tone(22_050, 440.0, i) * 16_000.0) as i16;
                vec![value, value]
            })
            .collect();
        write_aiff(&path, 22_050, 2, &frames);

        let decoded = decode(&path);
        assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia);
        assert_eq!(decoded.outcome.truncated, None);
        assert_len_within(
            decoded.samples.len(),
            expected_len(frames.len(), 22_050),
            1,
            "aiff",
        );
        let level = peak(middle(&decoded.samples));
        assert!((level - 0.488).abs() < 0.02, "peak {level}");

        let probe = probe(&path, &no_ffmpeg()).unwrap();
        assert_len_within(probe.duration_ms.unwrap() as usize, 3_000, 20, "probe");
    }

    #[test]
    fn cancel_stops_every_decoder() {
        let dir = tempfile::tempdir().unwrap();
        let fast = dir.path().join("fast.wav");
        write_wav_i16(&fast, 16_000, 1, 16_000 * 20);
        let slow = dir.path().join("slow.wav");
        write_wav_i16(&slow, 48_000, 2, 48_000 * 20);

        for path in [&fast, &slow] {
            let cancel = AtomicBool::new(true);
            let result = decode_to_16k_mono(path, &no_ffmpeg(), &cancel, &mut |_| Ok(()));
            assert!(matches!(result, Err(DecodeError::Cancelled)));

            let cancel = AtomicBool::new(false);
            let mut received = 0;
            let result = decode_to_16k_mono(path, &no_ffmpeg(), &cancel, &mut |block| {
                received += block.len();
                cancel.store(true, Ordering::Relaxed);
                Ok(())
            });
            assert!(matches!(result, Err(DecodeError::Cancelled)));
            assert!(received < 16_000 * 20);
        }
    }

    #[test]
    fn sink_errors_are_returned_as_failures() {
        let dir = tempfile::tempdir().unwrap();
        let fast = dir.path().join("fast.wav");
        write_wav_i16(&fast, 16_000, 1, 16_000 * 10);
        let slow = dir.path().join("slow.wav");
        write_wav_i16(&slow, 44_100, 1, 44_100 * 10);

        for path in [&fast, &slow] {
            let mut calls = 0;
            let result =
                decode_to_16k_mono(path, &no_ffmpeg(), &AtomicBool::new(false), &mut |_| {
                    calls += 1;
                    if calls == 2 {
                        anyhow::bail!("receiver dropped");
                    }
                    Ok(())
                });
            match result {
                Err(DecodeError::Failed(err)) => assert_eq!(err.to_string(), "receiver dropped"),
                other => panic!("{}: {other:?}", path.display()),
            }
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn very_short_files_decode() {
        let dir = tempfile::tempdir().unwrap();
        // Shorter than one resampler block: everything comes out in the flush.
        let short = dir.path().join("blip.wav");
        write_wav_i16(&short, 44_100, 1, 2_205);
        let decoded = decode_with(&short, &no_ffmpeg()).unwrap();
        assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia);
        assert_eq!(decoded.samples.len(), 800);

        let tiny = dir.path().join("tiny.wav");
        write_wav_i16(&tiny, 16_000, 1, 100);
        let decoded = decode_with(&tiny, &no_ffmpeg()).unwrap();
        assert_eq!(decoded.outcome.decoder, DecoderKind::Wav);
        assert_eq!(decoded.samples.len(), 100);
    }

    #[test]
    fn rate_changes_rebuild_the_resampler() {
        // As with HE-AAC, only the rate of the decoded blocks counts.
        let mut to_16k = To16k::default();
        let mut out = Vec::new();
        let segments = [(44_100u32, 44_100 + 17), (22_050, 33_075), (48_000, 1_000)];
        for (rate, frames) in segments {
            let samples: Vec<f32> = (0..frames)
                .map(|i| (tone(rate, 1_000.0, i) * 0.5) as f32)
                .collect();
            for block in samples.chunks(1_024) {
                out.extend_from_slice(to_16k.push(rate, block).unwrap());
            }
        }
        out.extend_from_slice(to_16k.finish().unwrap());
        let want: usize = segments
            .iter()
            .map(|&(rate, frames)| expected_len(frames, rate))
            .sum();
        assert_eq!(out.len(), want);
        // Both long segments carry the tone at its level.
        for range in [4_000..12_000, 20_000..34_000] {
            let level = peak(&out[range.clone()]);
            assert!((level - 0.5).abs() < 0.02, "{range:?}: {level}");
        }
    }

    #[test]
    fn display_is_readable_english() {
        assert_eq!(
            DecodeError::Unsupported(".xyz files are not supported".into()).to_string(),
            "unsupported file: .xyz files are not supported"
        );
        assert_eq!(DecodeError::Cancelled.to_string(), "decoding was cancelled");
        assert_eq!(
            DecodeError::Failed(anyhow::anyhow!("inner").context("outer")).to_string(),
            "decoding failed: outer: inner"
        );
        assert_eq!(
            DecodeError::NotFound("/x.wav does not exist".into()).to_string(),
            "/x.wav does not exist"
        );
    }

    #[test]
    fn duration_uses_the_time_base() {
        let mut params = CodecParameters::new();
        params.with_sample_rate(44_100).with_n_frames(44_100 * 3);
        assert_eq!(duration_ms(&params), Some(3_000));
        // mp4: frames counted in the media timescale, whatever the decoded rate.
        params.with_time_base(symphonia::core::units::TimeBase::new(1, 1_000));
        params.with_n_frames(61_500);
        assert_eq!(duration_ms(&params), Some(61_500));
        params.n_frames = None;
        assert_eq!(duration_ms(&params), None);
    }

    #[test]
    fn timeline_fills_gaps_but_not_jitter() {
        let mut params = CodecParameters::new();
        params
            .with_sample_rate(48_000)
            .with_time_base(symphonia::core::units::TimeBase::new(1, 48_000));
        let mut timeline = Timeline::new(&params, 0);
        // The first packet sets the clock, whatever its timestamp.
        let start = 1_000_000;
        assert_eq!(timeline.silence_before(start, 0), 0);
        // On time or early (priming): nothing.
        assert_eq!(timeline.silence_before(start + 1_024, 341), 0);
        assert_eq!(timeline.silence_before(start + 2_048, 2_000), 0);
        // 1 s in: 90 ms late is left alone, 150 ms late is filled exactly.
        assert_eq!(timeline.silence_before(start + 48_000 + 4_320, 16_000), 0);
        assert_eq!(
            timeline.silence_before(start + 48_000 + 7_200, 16_000),
            2_400
        );
        // Small deficits add up until they count.
        assert_eq!(timeline.silence_before(start + 96_000, 30_500), 0);
        assert_eq!(timeline.silence_before(start + 96_000, 30_300), 1_700);
        // A timestamp that goes back restarts the clock there.
        assert_eq!(timeline.silence_before(5, 40_000), 0);
        assert_eq!(timeline.silence_before(5 + 96_000, 40_000 + 16_000), 16_000);
        // So does a jump of hours: a broken timestamp, not a pause.
        assert_eq!(timeline.silence_before(5 + 48_000 * 7_200, 72_000), 0);
        assert_eq!(
            timeline.silence_before(5 + 48_000 * 7_201, 72_000 + 16_000),
            0
        );

        // An mp4 track's lead comes before its first packet.
        assert_eq!(Timeline::new(&params, 8_000).silence_before(0, 0), 8_000);
        assert_eq!(Timeline::new(&params, 800).silence_before(0, 0), 0);
        // Without a time base nothing is known.
        let mut timeline = Timeline::new(&CodecParameters::new(), 8_000);
        assert_eq!(timeline.silence_before(0, 0), 0);
        assert_eq!(timeline.silence_before(1 << 40, 10), 0);
    }

    /// An mp4 atom with `body`.
    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut atom = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        atom.extend_from_slice(kind);
        atom.extend_from_slice(body);
        atom
    }

    #[test]
    fn mp4_leads_come_from_leading_empty_edits() {
        // mvhd version 0: timescale 1000.
        let mut mvhd = vec![0u8; 12];
        mvhd.extend_from_slice(&1_000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0u8; 84]);
        // Track 0: 500 ms of empty edits, then media. Version 0 entries.
        let mut elst0 = vec![0u8, 0, 0, 0, 0, 0, 0, 3];
        for (duration, media_time) in [(300u32, -1i32), (200, -1), (2_000, 0)] {
            elst0.extend_from_slice(&duration.to_be_bytes());
            elst0.extend_from_slice(&media_time.to_be_bytes());
            elst0.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        }
        // Track 1: 250 ms. Version 1 entries.
        let mut elst1 = vec![1u8, 0, 0, 0, 0, 0, 0, 1];
        elst1.extend_from_slice(&250u64.to_be_bytes());
        elst1.extend_from_slice(&(-1i64).to_be_bytes());
        elst1.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        // Track 2: no edit list, so it starts at 0.
        let moov = atom(
            b"moov",
            &[
                atom(b"mvhd", &mvhd),
                atom(b"trak", &atom(b"edts", &atom(b"elst", &elst0))),
                atom(b"trak", &atom(b"edts", &atom(b"elst", &elst1))),
                atom(b"trak", &atom(b"tkhd", &[0u8; 84])),
            ]
            .concat(),
        );
        // A 64-bit sized mdat in front, as in long recordings.
        let mut mdat = 1u32.to_be_bytes().to_vec();
        mdat.extend_from_slice(b"mdat");
        mdat.extend_from_slice(&(16u64 + 100).to_be_bytes());
        mdat.extend_from_slice(&[0u8; 100]);
        let file = [atom(b"ftyp", b"M4A \0\0\0\0"), mdat, moov].concat();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.m4a");
        std::fs::write(&path, &file).unwrap();
        assert_eq!(mp4_lead(&path, 0), 8_000);
        assert_eq!(mp4_lead(&path, 1), 4_000);
        assert_eq!(mp4_lead(&path, 2), 0);
        assert_eq!(mp4_lead(&path, 3), 0);
        // Leads count from the earliest track.
        let without_track_2 = file.len() - atom(b"trak", &atom(b"tkhd", &[0u8; 84])).len();
        let mut trimmed = file[..without_track_2].to_vec();
        let moov_at = trimmed.windows(4).rposition(|w| w == b"moov").unwrap() - 4;
        let moov_len = (trimmed.len() - moov_at) as u32;
        trimmed[moov_at..moov_at + 4].copy_from_slice(&moov_len.to_be_bytes());
        std::fs::write(&path, &trimmed).unwrap();
        assert_eq!(mp4_lead(&path, 0), 4_000);
        assert_eq!(mp4_lead(&path, 1), 0);
        // Other files are not parsed; cut-short ones give no lead.
        let wav = dir.path().join("clip.wav");
        std::fs::write(&wav, &file).unwrap();
        assert_eq!(mp4_lead(&wav, 0), 0);
        std::fs::write(&path, &file[..file.len() - 10]).unwrap();
        assert_eq!(mp4_lead(&path, 0), 0);
    }

    #[test]
    fn downmix_averages_and_scrubs() {
        let mut mono = Vec::new();
        downmix(
            &[0.5, 0.25, f32::NAN, 1.0, f32::MAX, f32::MAX],
            2,
            &mut mono,
        );
        assert_eq!(mono, vec![0.375, 0.5, 0.0]);
        downmix(&[0.1, f32::INFINITY], 1, &mut mono);
        assert_eq!(mono, vec![0.1, 0.0]);
    }

    // ---- Fixtures made with ffmpeg; skipped when it is not installed. ----

    /// `dir/name` made by ffmpeg from `args`, or None when this ffmpeg build
    /// cannot make it.
    fn make(ffmpeg: &Path, dir: &Path, name: &str, args: &[&str]) -> Option<PathBuf> {
        let path = dir.join(name);
        let mut full: Vec<&str> = args.to_vec();
        full.push(path.to_str()?);
        make_fixture(ffmpeg, &full).then_some(path)
    }

    /// symphonia 0.5 ignores the mp4 edit list, so decoded AAC keeps the
    /// encoder's priming frame (1024 samples) and the last frame's padding:
    /// up to 2048 samples at 44.1 kHz, 46.4 ms.
    const AAC_SLACK: usize = 47 * 16;

    const TONE_2MIN_STEREO: &[&str] = &[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=44100:duration=120",
        "-ac",
        "2",
    ];

    #[test]
    fn two_minute_mp3_and_m4a_keep_their_length() {
        let Some(ffmpeg) = ffmpeg_or_skip("two_minute_mp3_and_m4a_keep_their_length") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let want = expected_len(44_100 * 120, 44_100);
        let cases: [(&str, &[&str], usize); 2] = [
            // Gapless MP3 (LAME header): encoder delay and padding are trimmed.
            ("talk.mp3", &["-c:a", "libmp3lame", "-b:a", "128k"], 1),
            // Only ffmpeg's 1024-sample priming and 32 samples of padding are
            // extra here (24 ms); see AAC_SLACK.
            ("talk.m4a", &["-c:a", "aac", "-b:a", "128k"], 30 * 16),
        ];
        for (name, codec, slack) in cases {
            let Some(path) = make(
                &ffmpeg,
                dir.path(),
                name,
                &[TONE_2MIN_STEREO, codec].concat(),
            ) else {
                continue;
            };
            let decoded = decode(&path);
            assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia, "{name}");
            assert_eq!(decoded.outcome.truncated, None, "{name}");
            assert_len_within(decoded.samples.len(), want, slack, name);
            // Streamed in small blocks, never buffered whole.
            assert!(decoded.largest_block < 16_000, "{}", decoded.largest_block);

            let probe = probe(&path, &DecodeOptions::default()).unwrap();
            assert_len_within(probe.duration_ms.unwrap() as usize, 120_000, 50, name);
        }
    }

    #[test]
    fn common_formats_decode_through_their_route() {
        let Some(ffmpeg) = ffmpeg_or_skip("common_formats_decode_through_their_route") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let source: &[&str] = &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=5",
        ];
        let cases: [(&str, &[&str], DecoderKind); 6] = [
            ("a.flac", &[], DecoderKind::Symphonia),
            ("b.ogg", &["-c:a", "libvorbis"], DecoderKind::Symphonia),
            ("c.oga", &["-c:a", "flac"], DecoderKind::Symphonia),
            // ffmpeg's default for mono MP2 (384 kb/s) is not a legal Layer II
            // mode, so make a valid stereo stream.
            (
                "d.mp2",
                &["-ac", "2", "-c:a", "mp2", "-b:a", "192k"],
                DecoderKind::Symphonia,
            ),
            ("e.opus", &["-c:a", "libopus"], DecoderKind::Ffmpeg),
            ("f.webm", &["-c:a", "libopus"], DecoderKind::Ffmpeg),
        ];
        for (name, codec, decoder) in cases {
            let Some(path) = make(&ffmpeg, dir.path(), name, &[source, codec].concat()) else {
                continue;
            };
            let decoded = decode(&path);
            assert_eq!(decoded.outcome.decoder, decoder, "{name}");
            assert_eq!(decoded.outcome.truncated, None, "{name}");
            assert_len_within(decoded.samples.len(), 80_000, 30 * 16, name);
            assert!(peak(middle(&decoded.samples)) > 0.05, "{name}");

            let probe = probe(&path, &DecodeOptions::default()).unwrap();
            assert_len_within(probe.duration_ms.unwrap_or(0) as usize, 5_000, 30, name);
        }
    }

    #[test]
    fn codecs_symphonia_lacks_fall_back_to_ffmpeg() {
        let Some(ffmpeg) = ffmpeg_or_skip("codecs_symphonia_lacks_fall_back_to_ffmpeg") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let source: &[&str] = &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=16000:duration=4",
        ];
        // IMA ADPCM pads its last block of 2041 samples with silence.
        let cases: [(&str, &str, &str, usize); 3] = [
            ("dictaphone.wav", "adpcm_ima_wav", "ADPCM", 2_041),
            ("lossless.m4a", "alac", "ALAC", 30 * 16),
            ("voice.ogg", "libopus", "Opus", 30 * 16),
        ];
        for (name, encoder, codec, slack) in cases {
            let args = [source, &["-c:a", encoder]].concat();
            let Some(path) = make(&ffmpeg, dir.path(), name, &args) else {
                continue;
            };
            let decoded = decode(&path);
            assert_eq!(decoded.outcome.decoder, DecoderKind::Ffmpeg, "{codec}");
            assert_len_within(decoded.samples.len(), 64_000, slack, codec);

            let err = decode_with(&path, &no_ffmpeg()).err().unwrap();
            match &err {
                DecodeError::NeedsFfmpeg(message) => {
                    assert_eq!(message, &format!("{codec} audio needs ffmpeg"))
                }
                other => panic!("{codec}: {other}"),
            }
            let err = probe(&path, &no_ffmpeg()).unwrap_err();
            assert!(matches!(err, DecodeError::NeedsFfmpeg(_)), "{codec}: {err}");
            let probe = probe(&path, &DecodeOptions::default()).unwrap();
            let duration = probe.duration_ms.unwrap_or(0) as usize;
            assert_len_within(duration, 4_000, slack / 16 + 20, codec);
        }
    }

    #[test]
    fn video_files_use_their_audio_track() {
        let Some(ffmpeg) = ffmpeg_or_skip("video_files_use_their_audio_track") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let video = &["-f", "lavfi", "-i", "testsrc=duration=4:size=64x48:rate=10"];
        let audio = &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=4",
        ];
        for name in ["clip.mp4", "clip.mov"] {
            // The video track comes first, so it is symphonia's default track.
            let args = [
                &video[..],
                &audio[..],
                &["-map", "0:v", "-map", "1:a", "-c:v", "mpeg4", "-c:a", "aac"],
            ]
            .concat();
            let Some(path) = make(&ffmpeg, dir.path(), name, &args) else {
                return;
            };
            let decoded = decode_with(&path, &no_ffmpeg()).unwrap();
            assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia, "{name}");
            assert_len_within(decoded.samples.len(), 64_000, AAC_SLACK, name);
        }

        let args = [&video[..], &["-c:v", "mpeg4"]].concat();
        let Some(silent_video) = make(&ffmpeg, dir.path(), "silent.mp4", &args) else {
            return;
        };
        for opts in [no_ffmpeg(), DecodeOptions::default()] {
            let err = probe(&silent_video, &opts).unwrap_err();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
            let err = decode_with(&silent_video, &opts).err().unwrap();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
        }
    }

    /// Peak level of `samples[from_ms..to_ms]`.
    fn peak_between(samples: &[f32], from_ms: usize, to_ms: usize) -> f32 {
        peak(&samples[from_ms * 16..to_ms * 16])
    }

    #[test]
    fn delayed_audio_and_timestamp_gaps_keep_their_place() {
        let Some(ffmpeg) = ffmpeg_or_skip("delayed_audio_and_timestamp_gaps_keep_their_place")
        else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // Video from 0 s, a 2 s tone from 1.5 s: an edit list in mp4.
        let delayed: &[&str] = &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=4:size=64x48:rate=10",
            "-itsoffset",
            "1.5",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=2",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
        ];
        for name in ["delayed.mp4", "delayed.mov"] {
            let Some(path) = make(&ffmpeg, dir.path(), name, delayed) else {
                return;
            };
            let decoded = decode_with(&path, &no_ffmpeg()).unwrap();
            assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia, "{name}");
            assert_len_within(
                decoded.samples.len(),
                56_000 + AAC_SLACK / 2,
                AAC_SLACK,
                name,
            );
            assert!(peak_between(&decoded.samples, 0, 1_400) < 1e-3, "{name}");
            assert!(
                peak_between(&decoded.samples, 1_600, 3_400) > 0.05,
                "{name}"
            );
            let probe = probe(&path, &no_ffmpeg()).unwrap();
            assert_len_within(probe.duration_ms.unwrap() as usize, 3_500, 50, name);
        }

        // 2 s of tone, a 3 s hole in the timestamps, 2 s of tone: a long
        // sample duration in mp4, a granule jump in Ogg.
        let source: &[&str] = &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=4",
            "-af",
            "asetpts='PTS+gte(T,2)*3/TB'",
        ];
        let cases: [(&str, &str, usize); 2] = [
            ("gap.m4a", "aac", AAC_SLACK),
            ("gap.ogg", "libvorbis", 30 * 16),
        ];
        for (name, codec, slack) in cases {
            let args = [source, &["-c:a", codec]].concat();
            let Some(path) = make(&ffmpeg, dir.path(), name, &args) else {
                continue;
            };
            let decoded = decode_with(&path, &no_ffmpeg()).unwrap();
            assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia, "{name}");
            assert_len_within(decoded.samples.len(), 112_000 + slack / 2, slack, name);
            assert!(peak_between(&decoded.samples, 100, 1_900) > 0.05, "{name}");
            assert!(
                peak_between(&decoded.samples, 2_100, 4_900) < 1e-3,
                "{name}"
            );
            assert!(
                peak_between(&decoded.samples, 5_100, 6_900) > 0.05,
                "{name}"
            );
        }
    }

    #[test]
    fn m4a_without_its_index_is_unsupported() {
        let Some(ffmpeg) = ffmpeg_or_skip("m4a_without_its_index_is_unsupported") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let args = [
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=3",
            "-c:a",
            "aac",
        ];
        let Some(path) = make(&ffmpeg, dir.path(), "call.m4a", &args) else {
            return;
        };
        // An interrupted recording: the index (moov) at the end is missing,
        // whole or in part.
        let bytes = std::fs::read(&path).unwrap();
        let moov = bytes.windows(4).rposition(|w| w == b"moov").unwrap() - 4;
        for (name, cut) in [("no_moov.m4a", moov), ("cut.m4a", moov / 2)] {
            let cut_path = dir.path().join(name);
            std::fs::write(&cut_path, &bytes[..cut]).unwrap();
            for opts in [no_ffmpeg(), DecodeOptions::default()] {
                let err = probe(&cut_path, &opts).unwrap_err();
                assert!(matches!(err, DecodeError::Unsupported(_)), "{name}: {err}");
                let err = decode_with(&cut_path, &opts).err().unwrap();
                assert!(matches!(err, DecodeError::Unsupported(_)), "{name}: {err}");
            }
        }
    }

    #[test]
    fn symphonia_panics_become_errors() {
        let Some(ffmpeg) = ffmpeg_or_skip("symphonia_panics_become_errors") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let args = [
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=2",
            "-c:a",
            "aac",
        ];
        let Some(path) = make(&ffmpeg, dir.path(), "broken.m4a", &args) else {
            return;
        };
        // A zero media timescale makes symphonia's mp4 reader panic.
        let mut bytes = std::fs::read(&path).unwrap();
        let mdhd = bytes.windows(4).position(|w| w == b"mdhd").unwrap();
        let timescale = if bytes[mdhd + 4] == 1 {
            mdhd + 24
        } else {
            mdhd + 16
        };
        bytes[timescale..timescale + 4].fill(0);
        std::fs::write(&path, bytes).unwrap();

        let err = decode_with(&path, &no_ffmpeg()).err().unwrap();
        match &err {
            DecodeError::Failed(inner) => assert!(inner.to_string().contains("crashed"), "{err}"),
            other => panic!("{other}"),
        }
        let err = probe(&path, &no_ffmpeg()).unwrap_err();
        assert!(matches!(err, DecodeError::Failed(_)), "{err}");
    }

    #[test]
    fn corrupt_tails_after_audio_are_truncations() {
        let Some(ffmpeg) = ffmpeg_or_skip("corrupt_tails_after_audio_are_truncations") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // The m4a keeps its index (moov) in front, so only audio data is hit.
        let cases: [(&str, &[&str]); 3] = [
            ("tail.mp3", &["-c:a", "libmp3lame"]),
            ("tail.flac", &["-c:a", "flac"]),
            ("tail.m4a", &["-c:a", "aac", "-movflags", "+faststart"]),
        ];
        for (name, codec) in cases {
            let source: &[&str] = &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=6",
            ];
            let Some(path) = make(&ffmpeg, dir.path(), name, &[source, codec].concat()) else {
                continue;
            };
            let mut bytes = std::fs::read(&path).unwrap();
            let half = bytes.len() / 2;
            let mut state = 0x2545_f491_4f6c_dd1du64;
            for byte in &mut bytes[half..] {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = (state >> 24) as u8;
            }
            std::fs::write(&path, bytes).unwrap();

            // Never an error once audio has been produced, and never ffmpeg.
            let decoded =
                decode_with(&path, &no_ffmpeg()).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(decoded.outcome.decoder, DecoderKind::Symphonia, "{name}");
            assert!(
                decoded.samples.len() >= 16_000,
                "{name}: {}",
                decoded.samples.len()
            );
        }
    }

    #[test]
    fn ffmpeg_route_rejects_unreadable_files_at_probe() {
        let Some(_) = ffmpeg_or_skip("ffmpeg_route_rejects_unreadable_files_at_probe") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.webm");
        std::fs::write(&path, "not a webm file at all").unwrap();
        let err = probe(&path, &DecodeOptions::default()).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
        let err = decode_with(&path, &DecodeOptions::default()).err().unwrap();
        assert!(matches!(err, DecodeError::Failed(_)), "{err}");
    }
}
