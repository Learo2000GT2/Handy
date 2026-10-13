//! Sound levels for the meeting's meter and health checks, and the verdict
//! of the microphone check. Works on 16 kHz mono samples.

use crate::audio_toolkit::{EarshotVad, VoiceActivityDetector};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Lowest level reported, dBFS.
pub const FLOOR_DB: f32 = -100.0;
/// A sample at or above this is clipped.
const CLIP: f32 = 0.999;

pub fn to_db(amplitude: f32) -> f32 {
    if amplitude <= 1e-5 {
        FLOOR_DB
    } else {
        (20.0 * amplitude.log10()).max(FLOOR_DB)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    pub rms_db: f32,
    pub peak_db: f32,
}

/// Running level over fixed windows, plus the current run of exact zeros
/// (a muted endpoint or blocked microphone delivers exact zeros, while a
/// working microphone in a quiet room never does).
pub struct LevelMeter {
    window: usize,
    count: usize,
    sum_sq: f64,
    peak: f32,
    zero_run: u64,
}

impl LevelMeter {
    pub fn new(window_samples: usize) -> Self {
        Self {
            window: window_samples.max(1),
            count: 0,
            sum_sq: 0.0,
            peak: 0.0,
            zero_run: 0,
        }
    }

    /// Feeds samples; returns the level of each window completed, the last
    /// one if several completed.
    pub fn push(&mut self, samples: &[f32]) -> Option<Level> {
        let mut done = None;
        for &sample in samples {
            let sample = if sample.is_finite() { sample } else { 0.0 };
            if sample == 0.0 {
                self.zero_run += 1;
            } else {
                self.zero_run = 0;
            }
            self.sum_sq += f64::from(sample) * f64::from(sample);
            self.peak = self.peak.max(sample.abs());
            self.count += 1;
            if self.count == self.window {
                let rms = (self.sum_sq / self.count as f64).sqrt() as f32;
                done = Some(Level {
                    rms_db: to_db(rms),
                    peak_db: to_db(self.peak),
                });
                self.count = 0;
                self.sum_sq = 0.0;
                self.peak = 0.0;
            }
        }
        done
    }

    /// Consecutive exact-zero samples up to now.
    pub fn zero_run_samples(&self) -> u64 {
        self.zero_run
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum MicCheckVerdict {
    /// Speech heard at a usable level.
    Good,
    /// Only exact silence: muted, or microphone access is blocked.
    NoSignal,
    /// Sound, but almost no speech.
    NoSpeech,
    /// Speech, but very quiet.
    TooQuiet,
    /// Speech loud enough to distort.
    TooLoud,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
pub struct MicCheckStats {
    pub verdict: MicCheckVerdict,
    pub peak_db: f32,
    /// Seconds of speech heard.
    pub speech_seconds: f32,
}

/// Seconds of speech needed for a good check.
const MIN_SPEECH_SECONDS: f32 = 1.5;
/// Speech peaks below this are too quiet to transcribe reliably.
const QUIET_PEAK_DB: f32 = -30.0;
/// Share of clipped samples that distorts speech.
const MAX_CLIPPED_SHARE: f32 = 0.002;

/// Judges a microphone check recording (16 kHz mono).
pub fn judge_mic_check(samples: &[f32]) -> MicCheckStats {
    let peak = samples
        .iter()
        .filter(|s| s.is_finite())
        .fold(0.0f32, |peak, s| peak.max(s.abs()));
    let peak_db = to_db(peak);
    let nonzero = samples.iter().filter(|s| **s != 0.0).count();

    let mut speech_frames = 0usize;
    let mut frame_samples = 256usize;
    if let Ok(mut vad) = EarshotVad::new(0.5) {
        frame_samples = vad.frame_samples();
        for frame in samples.chunks_exact(frame_samples) {
            if vad.is_voice(frame).unwrap_or(false) {
                speech_frames += 1;
            }
        }
    }
    let speech_seconds = (speech_frames * frame_samples) as f32 / 16_000.0;
    let clipped = samples.iter().filter(|s| s.abs() >= CLIP).count();

    let verdict = if nonzero * 100 < samples.len() {
        MicCheckVerdict::NoSignal
    } else if speech_seconds < MIN_SPEECH_SECONDS {
        MicCheckVerdict::NoSpeech
    } else if (clipped as f32) > MAX_CLIPPED_SHARE * samples.len() as f32 {
        MicCheckVerdict::TooLoud
    } else if peak_db < QUIET_PEAK_DB {
        MicCheckVerdict::TooQuiet
    } else {
        MicCheckVerdict::Good
    };
    MicCheckStats {
        verdict,
        peak_db,
        speech_seconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decibels() {
        assert_eq!(to_db(1.0), 0.0);
        assert!((to_db(0.1) + 20.0).abs() < 1e-4);
        assert_eq!(to_db(0.0), FLOOR_DB);
        assert_eq!(to_db(1e-9), FLOOR_DB);
    }

    #[test]
    fn meter_reports_each_window() {
        let mut meter = LevelMeter::new(1600);
        assert_eq!(meter.push(&[0.5; 1000]), None);
        let level = meter.push(&[0.5; 600]).unwrap();
        assert!((level.rms_db - to_db(0.5)).abs() < 1e-3);
        assert!((level.peak_db - to_db(0.5)).abs() < 1e-3);
        let level = meter.push(&[0.0; 1600]).unwrap();
        assert_eq!(level.peak_db, FLOOR_DB);
    }

    #[test]
    fn meter_counts_exact_zero_runs() {
        let mut meter = LevelMeter::new(1600);
        meter.push(&[0.0; 500]);
        assert_eq!(meter.zero_run_samples(), 500);
        meter.push(&[0.001]);
        assert_eq!(meter.zero_run_samples(), 0);
        meter.push(&[0.0, f32::NAN]);
        assert_eq!(meter.zero_run_samples(), 2);
    }

    #[test]
    fn mic_check_needs_a_signal_and_speech() {
        assert_eq!(
            judge_mic_check(&vec![0.0; 160_000]).verdict,
            MicCheckVerdict::NoSignal
        );
        // Faint steady hiss: a working microphone, but nobody spoke.
        let hiss: Vec<f32> = (0..160_000)
            .map(|i| if i % 2 == 0 { 0.002 } else { -0.002 })
            .collect();
        assert_eq!(judge_mic_check(&hiss).verdict, MicCheckVerdict::NoSpeech);
    }
}
