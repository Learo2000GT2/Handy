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

use crate::file_transcription::{
    ffmpeg::{self, InputInfo},
    formats::{self, DecodeRoute},
    resample::StreamResampler,
    types::DecoderKind,
};
use log::debug;
use std::fs::File;
use std::io::ErrorKind;
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
                        Unusable::Unrecognised(_) | Unusable::NoAudioTrack => {
                            probe_with_ffmpeg(ffmpeg, path)?
                        }
                        Unusable::Codec { duration_ms, .. } => {
                            duration_ms.or_else(|| ffmpeg::probe_duration_ms(ffmpeg, path))
                        }
                        Unusable::Corrupt(_) => ffmpeg::probe_duration_ms(ffmpeg, path),
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
    let Some(ffmpeg) = ffmpeg::find_ffmpeg(opts.ffmpeg.as_deref()) else {
        return Err(unusable.into_error(path));
    };
    match decode_ffmpeg(&ffmpeg, path, cancel, &mut sink) {
        Ok(outcome) => Ok(outcome),
        Err(Stop::Unusable(err)) => Err(unusable.with_ffmpeg_error(err)),
        Err(Stop::Halted(err)) => Err(err),
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
    let ext = formats::lowercase_extension(path).unwrap_or_default();
    DecodeError::NeedsFfmpeg(format!("{ext} files need ffmpeg"))
}

fn is_wav(path: &Path) -> bool {
    matches!(
        formats::lowercase_extension(path).as_deref(),
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
    /// No container format recognised (or its header is unreadable).
    Unrecognised(String),
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
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unrecognised(detail) => write!(f, "unrecognised format ({detail})"),
            Self::NoAudioTrack => f.write_str("no audio track"),
            Self::Codec { name, detail, .. } => {
                write!(
                    f,
                    "{} audio cannot be decoded ({detail})",
                    name.unwrap_or("this")
                )
            }
            Self::Corrupt(detail) => f.write_str(detail),
        }
    }
}

impl Unusable {
    /// The error when no ffmpeg is available to fall back to.
    fn into_error(self, path: &Path) -> DecodeError {
        match self {
            Self::Unrecognised(detail) => {
                let ext = formats::lowercase_extension(path).unwrap_or_default();
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
        }
    }

    /// The error when the ffmpeg fallback failed too, before any audio.
    fn with_ffmpeg_error(self, err: anyhow::Error) -> DecodeError {
        match self {
            Self::Unrecognised(detail) => DecodeError::Unsupported(format!(
                "not a readable audio file (symphonia: {detail}; {err:#})"
            )),
            Self::NoAudioTrack => {
                DecodeError::Unsupported(format!("the file has no audio track ({err:#})"))
            }
            unusable @ (Self::Codec { .. } | Self::Corrupt(_)) => {
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
        let rate = match guarded(|| input.next_block(&mut mono)) {
            Ok(Ok(Some(rate))) => rate,
            Ok(Ok(None)) => break None,
            Ok(Err(unusable)) => break Some(unusable),
            // The reader's state is unknown after a panic: stop using it.
            Err(panic) => break Some(Unusable::Corrupt(panic)),
        };
        match to_16k.push(rate, &mono) {
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
        return Err(Stop::Unusable(Unusable::Corrupt(truncated.unwrap_or_else(
            || "the audio track holds no samples".to_string(),
        ))));
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
}

impl To16k {
    /// Returns the 16 kHz audio that became available.
    fn push(&mut self, rate: u32, mono: &[f32]) -> anyhow::Result<&[f32]> {
        self.out.clear();
        if self.resampler.as_ref().map(StreamResampler::in_hz) != Some(rate) {
            if let Some(mut previous) = self.resampler.take() {
                previous.finish(&mut self.out)?;
            }
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
        if let Some(mut resampler) = self.resampler.take() {
            resampler.finish(&mut self.out)?;
        }
        Ok(&self.out)
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
        .map_err(|err| Unusable::Unrecognised(err.to_string()))?
        .format;

        let (track_id, params, decoder) = select_track(format.as_ref())?;
        Ok(Self {
            format,
            decoder,
            track_id,
            params,
            sample_buf: None,
            bad_packets: 0,
        })
    }

    fn duration_ms(&self) -> Option<u64> {
        duration_ms(&self.params)
    }

    /// Decodes up to the next packet of the selected track that holds audio,
    /// downmixed into `mono`. Returns its sample rate, or None at the end.
    fn next_block(&mut self, mono: &mut Vec<f32>) -> Result<Option<u32>, Unusable> {
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
                        return Ok(Some(spec.rate));
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
        self.track_id = track_id;
        self.params = params;
        self.decoder = decoder;
        Ok(())
    }
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

        let err = decode_with(&path, &no_ffmpeg()).err().unwrap();
        assert!(matches!(err, DecodeError::NeedsFfmpeg(_)), "{err}");
        let err = probe(&path, &no_ffmpeg()).unwrap_err();
        assert!(matches!(err, DecodeError::NeedsFfmpeg(_)), "{err}");

        if ffmpeg_or_skip("junk_named_wav_is_rejected (with ffmpeg)").is_some() {
            let err = decode_with(&path, &DecodeOptions::default()).err().unwrap();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
            let err = probe(&path, &DecodeOptions::default()).unwrap_err();
            assert!(matches!(err, DecodeError::Unsupported(_)), "{err}");
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
