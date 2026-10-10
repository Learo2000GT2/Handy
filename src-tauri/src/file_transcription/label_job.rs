//! The pure parts of a speaker-label job, which labels the speakers of a
//! finished transcript from its recording: whether the recording found is
//! the one that was transcribed, how much of it to diarize, putting the
//! diarizer's turns onto the transcript, and what a diarizer failure means
//! for the job. The job itself runs in `manager`.

use crate::file_transcription::decode::AudioProbe;
use crate::file_transcription::diarize::{DiarizeError, DIARIZER_MODEL_ID};
use crate::file_transcription::speakers::{self, MergeOptions, SpeakerTurn};
use crate::file_transcription::types::{
    FileJobErrorCode, FileTranscript, SpeakerJobState, TranscriptStatus,
};

/// How far apart the stored and the current modification time may be: FAT
/// and exFAT keep times to 2 s.
const MTIME_TOLERANCE_SECS: u64 = 2;
/// Without a stored size, the recording's length must match the
/// transcript's within this, or within 1 % of it if that is more.
const DURATION_TOLERANCE_MS: u64 = 2_000;
/// A partial transcript is diarized this far past its last passage, so the
/// diarizer hears its end in context.
const PARTIAL_MARGIN_MS: u64 = 60_000;

/// What was recorded about the recording when it was transcribed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceIdentity {
    pub size_bytes: Option<u64>,
    /// Unix seconds.
    pub modified_at: Option<i64>,
}

/// Why labelling left a transcript as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelSkip {
    /// The transcript has labels and they were not to be replaced.
    AlreadyLabelled,
    /// No passage matched a speaker.
    NoSpeech,
}

/// Whether `probe` (the file found now) is the recording that was
/// transcribed: the same size, and unless the user `picked` it, modified
/// within 2 s of the stored time. With no stored size, a known length must
/// match the transcript's. Err(English reason).
pub fn check_source(
    stored: SourceIdentity,
    probe: &AudioProbe,
    transcript_duration_ms: u64,
    picked: bool,
) -> Result<(), String> {
    if let Some(size) = stored.size_bytes {
        if size != probe.source_size_bytes {
            return Err(format!(
                "The recording is {} bytes now, but was {} bytes when it was transcribed",
                probe.source_size_bytes, size
            ));
        }
    }
    if !picked {
        if let (Some(then), Some(now)) = (stored.modified_at, probe.source_modified_at) {
            if then.abs_diff(now) > MTIME_TOLERANCE_SECS {
                return Err("The recording was modified after it was transcribed".to_string());
            }
        }
    }
    if stored.size_bytes.is_none() {
        if let Some(duration_ms) = probe.duration_ms {
            let tolerance = DURATION_TOLERANCE_MS.max(transcript_duration_ms / 100);
            if duration_ms.abs_diff(transcript_duration_ms) > tolerance {
                return Err(format!(
                    "The recording is {} s long, but the transcript is {} s long",
                    duration_ms / 1000,
                    transcript_duration_ms / 1000
                ));
            }
        }
    }
    Ok(())
}

/// How much of the recording to diarize: for a partial transcript, up to a
/// minute past its last passage; None (all of it) for a completed one.
pub fn diarize_limit(t: &FileTranscript) -> Option<u64> {
    if t.status == TranscriptStatus::Completed {
        return None;
    }
    let last = t.segments.last().or(t.paragraphs.last())?;
    Some(last.end_ms + PARTIAL_MARGIN_MS)
}

/// `t` labelled with the diarizer's `turns`, and how many speakers it has.
/// With `replace`, earlier labels (names included) are dropped first. `t`
/// itself is never changed.
pub fn label(
    t: &FileTranscript,
    turns: &[SpeakerTurn],
    replace: bool,
) -> Result<(FileTranscript, u32), LabelSkip> {
    if t.speakers.is_some() && !replace {
        return Err(LabelSkip::AlreadyLabelled);
    }
    let mut labelled = t.clone();
    if replace {
        speakers::clear_speakers(&mut labelled);
    }
    let options = MergeOptions::for_transcript(&labelled);
    let stats = speakers::apply_speakers(&mut labelled, turns, DIARIZER_MODEL_ID, &options);
    if stats.speakers == 0 {
        return Err(LabelSkip::NoSpeech);
    }
    Ok((labelled, stats.speakers))
}

/// The job error code and speaker state of a failed diarization. A decode
/// failure is `DecodeFailed`; anything else is `SpeakerLabelsFailed`, with
/// a missing model reported as skipped. (The caller ends a cancelled one as
/// cancelled before asking.)
pub fn diarize_failure(error: &DiarizeError) -> (FileJobErrorCode, SpeakerJobState) {
    let code = match error {
        DiarizeError::Decode(_) => FileJobErrorCode::DecodeFailed,
        DiarizeError::ModelMissing(_)
        | DiarizeError::TooLong { .. }
        | DiarizeError::Engine(_)
        | DiarizeError::Cancelled => FileJobErrorCode::SpeakerLabelsFailed,
    };
    (code, error.job_state())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::formats::DecodeRoute;
    use crate::file_transcription::types::{
        SpeakerSkipReason, TimingGrain, TranscriptSegment, TranscriptSpeaker, TranscriptSpeakers,
    };
    use std::path::PathBuf;

    fn probe(size: u64, modified_at: Option<i64>, duration_ms: Option<u64>) -> AudioProbe {
        AudioProbe {
            route: DecodeRoute::Symphonia,
            duration_ms,
            ffmpeg: None,
            source_size_bytes: size,
            source_modified_at: modified_at,
        }
    }

    fn identity(size_bytes: Option<u64>, modified_at: Option<i64>) -> SourceIdentity {
        SourceIdentity {
            size_bytes,
            modified_at,
        }
    }

    #[test]
    fn check_source_table() {
        let now = 1_700_000_000;
        let cases: [(SourceIdentity, AudioProbe, bool, bool, &str); 11] = [
            (
                identity(Some(100), Some(now)),
                probe(100, Some(now), None),
                false,
                true,
                "the same file",
            ),
            (
                identity(Some(100), Some(now)),
                probe(101, Some(now), None),
                false,
                false,
                "another size",
            ),
            (
                identity(Some(100), Some(now)),
                probe(101, Some(now), None),
                true,
                false,
                "another size, even when picked",
            ),
            (
                identity(Some(100), Some(now)),
                probe(100, Some(now + 3), None),
                false,
                false,
                "modified 3 s later",
            ),
            (
                identity(Some(100), Some(now)),
                probe(100, Some(now - 2), None),
                false,
                true,
                "2 s apart (FAT)",
            ),
            (
                identity(Some(100), Some(now)),
                probe(100, Some(now + 3_600), None),
                true,
                true,
                "modified, but picked by the user",
            ),
            (
                identity(Some(100), None),
                probe(100, Some(now), None),
                false,
                true,
                "no stored time",
            ),
            (
                identity(Some(100), Some(now)),
                probe(100, None, None),
                false,
                true,
                "no current time",
            ),
            (
                identity(None, None),
                probe(5, None, Some(65_000)),
                false,
                false,
                "no stored size, another length",
            ),
            (
                identity(None, None),
                probe(5, None, Some(61_900)),
                false,
                true,
                "no stored size, the same length",
            ),
            (
                identity(None, None),
                probe(5, None, None),
                false,
                true,
                "no stored size, unknown length",
            ),
        ];
        for (stored, probe, picked, ok, case) in cases {
            assert_eq!(
                check_source(stored, &probe, 60_000, picked).is_ok(),
                ok,
                "{case}"
            );
        }
        // 1 % of a long transcript.
        assert!(check_source(
            identity(None, None),
            &probe(5, None, Some(3_629_000)),
            3_600_000,
            false
        )
        .is_ok());
        assert!(check_source(
            identity(None, None),
            &probe(5, None, Some(3_640_000)),
            3_600_000,
            false
        )
        .is_err());
    }

    fn segment(start_ms: u64, end_ms: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment::new(start_ms, end_ms, text)
    }

    fn transcript() -> FileTranscript {
        let segments = vec![
            segment(0, 2_000, "Hello there."),
            segment(2_000, 4_000, "Hi."),
        ];
        FileTranscript {
            source_path: "/audio/talk.wav".to_string(),
            source_name: "talk.wav".to_string(),
            duration_ms: 600_000,
            model_id: "small".to_string(),
            model_name: None,
            language: Some("en".to_string()),
            status: TranscriptStatus::Completed,
            status_detail: None,
            timing: TimingGrain::Segment,
            text: "Hello there. Hi.".to_string(),
            paragraphs: vec![segment(0, 4_000, "Hello there. Hi.")],
            segments,
            created_at: 0,
            history_id: Some(3),
            speakers: None,
        }
    }

    fn turns() -> Vec<SpeakerTurn> {
        vec![
            SpeakerTurn {
                start_ms: 0,
                end_ms: 2_000,
                speaker: 5,
            },
            SpeakerTurn {
                start_ms: 2_000,
                end_ms: 4_000,
                speaker: 2,
            },
        ]
    }

    #[test]
    fn diarize_limit_covers_a_partial_transcript() {
        let mut t = transcript();
        assert_eq!(diarize_limit(&t), None);
        t.status = TranscriptStatus::Partial;
        assert_eq!(diarize_limit(&t), Some(64_000));
        // Timed by its paragraphs when it has no segments.
        t.segments.clear();
        t.paragraphs = vec![segment(0, 9_000, "Hello there. Hi.")];
        assert_eq!(diarize_limit(&t), Some(69_000));
    }

    #[test]
    fn label_adds_labels() {
        let t = transcript();
        let (labelled, speakers) = label(&t, &turns(), false).expect("label");
        assert_eq!(speakers, 2);
        assert_eq!(labelled.text, "Speaker 1: Hello there.\n\nSpeaker 2: Hi.");
        let labels = labelled.speakers.expect("labels");
        assert_eq!(labels.model, DIARIZER_MODEL_ID);
        assert_eq!(labels.speakers.len(), 2);
    }

    #[test]
    fn label_replaces_only_when_asked() {
        let (mut labelled, _) = label(&transcript(), &turns(), false).expect("label");
        labelled.speakers = Some(TranscriptSpeakers {
            speakers: vec![
                TranscriptSpeaker {
                    id: 1,
                    name: Some("Ann".to_string()),
                },
                TranscriptSpeaker {
                    id: 2,
                    name: Some("Bob".to_string()),
                },
            ],
            ..labelled.speakers.expect("labels")
        });

        assert_eq!(
            label(&labelled, &turns(), false).map(|(_, n)| n),
            Err(LabelSkip::AlreadyLabelled)
        );
        // Replacing drops the names.
        let (again, speakers) = label(&labelled, &turns(), true).expect("label again");
        assert_eq!(speakers, 2);
        assert!(again
            .speakers
            .expect("labels")
            .speakers
            .iter()
            .all(|speaker| speaker.name.is_none()));
        assert_eq!(again.text, "Speaker 1: Hello there.\n\nSpeaker 2: Hi.");
    }

    #[test]
    fn no_speech_leaves_the_input_untouched() {
        let t = transcript();
        let far = [SpeakerTurn {
            start_ms: 500_000,
            end_ms: 501_000,
            speaker: 1,
        }];
        let before = t.clone();
        assert_eq!(
            label(&t, &far, false).map(|(_, n)| n),
            Err(LabelSkip::NoSpeech)
        );
        assert_eq!(t, before);

        // Nor does labelling again find nothing to replace the labels with.
        let (labelled, _) = label(&t, &turns(), false).expect("label");
        assert_eq!(
            label(&labelled, &far, true).map(|(_, n)| n),
            Err(LabelSkip::NoSpeech)
        );
    }

    /// End to end with the real speaker model: the transcript the CLI made of
    /// a recording (`handy -f <rec> -o <transcript>.json`) is checked against
    /// the recording, labelled from a real diarization with progress, fixed
    /// (merge, change a passage, undo) and labelled again; then its first
    /// half, as a partial transcript, is labelled from only its span.
    /// `HANDY_TRANSCRIBE_WORKER_EXE=target/debug/handy HANDY_SPEAKER_MODEL=<…Q8_0.gguf>`
    /// `HANDY_LABEL_RECORDING=<rec> HANDY_LABEL_TRANSCRIPT=<transcript>.json`
    /// `cargo test --lib label_job -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn labels_and_fixes_a_real_transcript() {
        use crate::engine_supervisor::WorkToken;
        use crate::file_transcription::decode::{self, DecodeOptions};
        use crate::file_transcription::diarize::{
            start_diarization, DiarizeProgress, DiarizeRequest,
        };
        use crate::file_transcription::speakers::{
            merge_speakers, paragraph_ranges, reassign_turn, SpeakerState,
        };
        use std::sync::Arc;
        use std::time::Instant;

        let env_path = |name: &str| {
            PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")))
        };
        let transcript: FileTranscript = serde_json::from_slice(
            &std::fs::read(env_path("HANDY_LABEL_TRANSCRIPT")).expect("read the transcript"),
        )
        .expect("a FileTranscript");
        let recording = env_path("HANDY_LABEL_RECORDING");
        let probe = decode::probe(&recording, &DecodeOptions::default()).expect("probe");

        // The recording as a job stores it matches; another size never does.
        let stored = identity(Some(probe.source_size_bytes), probe.source_modified_at);
        check_source(stored, &probe, transcript.duration_ms, false).expect("the same recording");
        let other = identity(Some(probe.source_size_bytes + 1), None);
        assert!(check_source(other, &probe, transcript.duration_ms, true).is_err());

        let diarize = |limit_ms: Option<u64>| {
            let request = DiarizeRequest {
                path: recording.clone(),
                decode: DecodeOptions::default(),
                model_path: env_path("HANDY_SPEAKER_MODEL"),
                estimated_duration_ms: probe.duration_ms,
                limit_ms,
            };
            let started = Instant::now();
            let mut progress = Vec::new();
            let turns = start_diarization(request, WorkToken::new(), Arc::new(|| false))
                .wait_reporting(&mut |report| progress.push(report))
                .expect("diarize");
            println!(
                "diarized up to {limit_ms:?} ms in {:.1}s: {} turns, progress {progress:?}",
                started.elapsed().as_secs_f64(),
                turns.len()
            );
            let through: Vec<u64> = progress
                .iter()
                .filter_map(|report| match report {
                    DiarizeProgress::Diarized { through_ms } => Some(*through_ms),
                    _ => None,
                })
                .collect();
            assert!(progress.contains(&DiarizeProgress::Running));
            assert!(
                through.windows(2).all(|pair| pair[0] <= pair[1]),
                "{through:?}"
            );
            (turns, through.last().copied().unwrap_or(0))
        };

        // The whole recording.
        assert_eq!(diarize_limit(&transcript), None);
        let (turns, through_ms) = diarize(None);
        assert!(
            through_ms.abs_diff(transcript.duration_ms) <= 1,
            "{through_ms} of {}",
            transcript.duration_ms
        );
        let (labelled, count) = label(&transcript, &turns, false).expect("labels");
        let ids: Vec<u32> = labelled
            .speakers
            .as_ref()
            .map(|s| s.speakers.iter().map(|speaker| speaker.id).collect())
            .unwrap_or_default();
        println!("{count} speakers {ids:?}:\n{}", labelled.text);
        assert!(count >= 2, "{count}");
        assert!(paragraph_ranges(&labelled).is_some());

        // Fixes, each leaving turns that match the segments; undo restores.
        let mut fixed = labelled.clone();
        let before = SpeakerState::of(&fixed);
        merge_speakers(&mut fixed, ids[1], ids[0]).expect("merge");
        assert_eq!(
            fixed.speakers.as_ref().map(|s| s.speakers.len()),
            Some(ids.len() - 1)
        );
        assert!(paragraph_ranges(&fixed).is_some());
        // A new speaker is the highest current id + 1 (a merged-away id is
        // free again).
        let highest = fixed
            .speakers
            .as_ref()
            .and_then(|s| s.speakers.iter().map(|speaker| speaker.id).max())
            .unwrap_or(0);
        let first = fixed.paragraphs[0].clone();
        let new_id =
            reassign_turn(&mut fixed, 0, first.start_ms, first.speaker, None).expect("reassign");
        assert_eq!(new_id, highest + 1);
        assert!(paragraph_ranges(&fixed).is_some());
        println!("fixed:\n{}", fixed.text);
        before.apply_to(&mut fixed);
        assert_eq!(fixed, labelled);

        // Labelling again with the same turns gives the same labels.
        let (again, _) = label(&labelled, &turns, true).expect("label again");
        assert_eq!(again.text, labelled.text);

        // The first half as a partial transcript: diarized up to a minute
        // past its last passage only.
        let half = transcript.duration_ms / 2;
        let mut partial = transcript.clone();
        partial.status = TranscriptStatus::Partial;
        partial.segments.retain(|segment| segment.end_ms <= half);
        partial
            .paragraphs
            .retain(|paragraph| paragraph.end_ms <= half);
        let limit = diarize_limit(&partial).expect("a limit");
        let (turns, through_ms) = diarize(Some(limit));
        assert!(through_ms <= limit, "{through_ms} > {limit}");
        assert!(through_ms.abs_diff(limit.min(transcript.duration_ms)) <= 1);
        let (labelled_partial, count) = label(&partial, &turns, false).expect("labels");
        println!("partial, {count} speakers:\n{}", labelled_partial.text);
        assert!(count >= 1);
    }

    #[test]
    fn diarize_failure_mapping() {
        assert_eq!(
            diarize_failure(&DiarizeError::Decode("bad".to_string())),
            (
                FileJobErrorCode::DecodeFailed,
                SpeakerJobState::Failed {
                    message: "could not decode the audio: bad".to_string()
                }
            )
        );
        assert_eq!(
            diarize_failure(&DiarizeError::ModelMissing(PathBuf::from("/m.gguf"))),
            (
                FileJobErrorCode::SpeakerLabelsFailed,
                SpeakerJobState::Skipped {
                    reason: SpeakerSkipReason::ModelMissing
                }
            )
        );
        assert_eq!(
            diarize_failure(&DiarizeError::TooLong {
                duration_ms: 13 * 3_600_000
            })
            .0,
            FileJobErrorCode::SpeakerLabelsFailed
        );
        assert_eq!(
            diarize_failure(&DiarizeError::Engine("crashed".to_string())),
            (
                FileJobErrorCode::SpeakerLabelsFailed,
                SpeakerJobState::Failed {
                    message: "the speaker model failed: crashed".to_string()
                }
            )
        );
    }
}
