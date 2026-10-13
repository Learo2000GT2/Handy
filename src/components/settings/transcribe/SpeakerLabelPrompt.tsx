import React, { useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { Users } from "lucide-react";
import { toast } from "sonner";
import type { FileJobSnapshot, FileTranscript } from "@/bindings";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import {
  errorKey,
  formatClock,
  isFinishedStatus,
  isolateLtr,
  pickerExtensions,
  refocusTarget,
} from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { PROGRESS_CLASSES } from "./progress";
import {
  SpeakerModelDownload,
  speakerModelMegabytes,
} from "./SpeakerModelDownload";

interface SpeakerLabelPromptProps {
  transcript: FileTranscript;
  /** The newest speaker-label job for this transcript, if any. */
  labelJob: FileJobSnapshot | null;
  /**
   * The user asked to label a labelled transcript again; shows the speaker
   * model download if the model is missing.
   */
  relabelRequested?: boolean;
  /** Queues labelling of the transcript (`path`: a recording the user found). */
  onLabel: (replace: boolean, path?: string) => Promise<void>;
}

/**
 * Speaker labels for a finished transcript: "Label Speakers" (or the speaker
 * model download it needs), the progress of a label job with Cancel, and why
 * the last one failed, with "Find Recording…" when its recording is missing
 * or changed. For a labelled transcript only the progress, the outcome and
 * a needed download show; SpeakerPanel shows the rest.
 */
export const SpeakerLabelPrompt: React.FC<SpeakerLabelPromptProps> = ({
  transcript,
  labelJob,
  relabelRequested = false,
  onLabel,
}) => {
  const { t } = useTranslation();
  const titleId = useId();
  const model = useFileTranscriptionStore((state) => state.speakerModel);
  const downloading = useFileTranscriptionStore(
    (state) => state.speakerDownload !== null,
  );
  const formats = useFileTranscriptionStore((state) => state.formats);
  const cancel = useFileTranscriptionStore((state) => state.cancel);
  const [starting, setStarting] = useState(false);

  const replace = transcript.speakers !== null;
  const labelling = labelJob !== null && !isFinishedStatus(labelJob.status);
  // Unknown until loaded: offer labelling and let the backend say.
  const modelMissing = downloading || model?.downloaded === false;
  const failure =
    !labelling && labelJob?.status === "failed" ? labelJob.error : null;
  // A missing model is offered as its download rather than as an error.
  const failedForModel =
    failure?.code === "speaker_labels_failed" &&
    labelJob?.speakers?.state === "skipped" &&
    labelJob.speakers.reason === "model_missing";
  const noSpeech =
    !labelling &&
    labelJob?.status === "completed" &&
    labelJob.speakers?.state === "skipped" &&
    labelJob.speakers.reason === "no_speech";
  const offerModel =
    !labelling &&
    modelMissing &&
    (!replace || relabelRequested || failedForModel);
  const offerLabel = !labelling && !replace && !modelMissing;

  // The section's buttons are replaced as labelling goes on (Label Speakers,
  // Cancel, Find Recording…, the model download), and a removed button takes
  // focus with it, so focus then moves to the section's new first button.
  const sectionRef = useRef<HTMLDivElement>(null);
  const focusInSectionRef = useRef(false);
  const kind = labelling
    ? "progress"
    : `${offerModel ? "model" : ""}${offerLabel ? "label" : ""}${failure ? failure.code : ""}`;
  useEffect(() => {
    const active = document.activeElement;
    const target = refocusTarget(
      focusInSectionRef.current,
      active === null || active === document.body,
      sectionRef.current?.querySelector<HTMLElement>("button"),
      null,
    );
    target?.focus();
  }, [kind, downloading]);

  const label = async (path?: string) => {
    if (starting) return;
    setStarting(true);
    try {
      await onLabel(replace, path);
    } finally {
      setStarting(false);
    }
  };

  const findRecording = async () => {
    const extensions = formats ? pickerExtensions(formats) : [];
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        title: t("settings.transcribeFiles.speakers.findRecordingHint"),
        filters:
          extensions.length > 0
            ? [
                {
                  name: t("settings.transcribeFiles.picker.filterName"),
                  extensions,
                },
              ]
            : undefined,
      });
      if (typeof selected === "string") await label(selected);
    } catch (error) {
      console.error("Failed to choose the recording:", error);
      toast.error(
        t("settings.transcribeFiles.speakers.labelError", {
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    }
  };

  let progress: React.ReactNode = null;
  if (labelling && labelJob !== null) {
    const status =
      labelJob.status === "queued"
        ? t("settings.transcribeFiles.status.queued")
        : labelJob.status === "waiting"
          ? t("settings.transcribeFiles.status.waiting")
          : t("settings.transcribeFiles.speakers.state.running");
    const duration =
      labelJob.duration_ms !== null && labelJob.duration_ms > 0
        ? labelJob.duration_ms
        : null;
    const processed = labelJob.processed_ms;
    const progressText =
      duration !== null && processed > 0
        ? t("settings.transcribeFiles.progress", {
            processed: isolateLtr(formatClock(processed)),
            total: isolateLtr(formatClock(duration)),
          })
        : null;
    progress = (
      <div className="flex flex-col gap-2">
        <div className="flex items-center justify-between gap-3">
          <p
            className="text-xs text-text/70 flex flex-wrap gap-x-2"
            role="status"
          >
            <span>{status}</span>
            {progressText && (
              <span className="tabular-nums">{progressText}</span>
            )}
          </p>
          <Button
            type="button"
            variant="secondary"
            size="sm"
            onClick={() => void cancel(labelJob.job_id)}
          >
            {t("settings.transcribeFiles.speakers.cancel")}
          </Button>
        </div>
        {/* Pulses until the first window is done (all of a short file). */}
        <progress
          className={`${PROGRESS_CLASSES} ${processed === 0 ? "animate-pulse" : ""}`}
          max={duration ?? undefined}
          value={
            duration === null || processed === 0
              ? undefined
              : Math.min(processed, duration)
          }
          aria-label={t("settings.transcribeFiles.speakers.label")}
          aria-valuetext={progressText ?? status}
        />
      </div>
    );
  }

  const content = (
    <>
      {progress}
      {failure && !failedForModel && (
        <Alert variant="error">
          <div className="flex flex-col items-start gap-2">
            <span title={failure.message}>
              {t("settings.transcribeFiles.speakers.labelError", {
                error: t(
                  `settings.transcribeFiles.errors.${errorKey(failure.code)}`,
                ),
              })}
            </span>
            {(failure.code === "not_found" ||
              failure.code === "source_changed") && (
              <Button
                type="button"
                variant="secondary"
                size="sm"
                aria-disabled={starting}
                onClick={() => void findRecording()}
              >
                {t("settings.transcribeFiles.speakers.findRecording")}
              </Button>
            )}
          </div>
        </Alert>
      )}
      {noSpeech && (
        <p className="text-xs text-text/60">
          {t("settings.transcribeFiles.speakers.state.skipped.noSpeech")}
        </p>
      )}
      {offerModel && (
        <div className="flex flex-col gap-1">
          <p className="text-sm text-text/70">
            {t("settings.transcribeFiles.speakers.needsModel", {
              size: speakerModelMegabytes(model),
            })}
          </p>
          <SpeakerModelDownload className="py-1" />
        </div>
      )}
      {offerLabel && (
        <div className="flex flex-col items-start gap-2">
          <p className="text-sm text-text/70">
            {t("settings.transcribeFiles.speakers.none")}
          </p>
          <Button
            type="button"
            variant="primary-soft"
            size="sm"
            className="flex items-center gap-1.5"
            aria-disabled={starting}
            onClick={() => void label()}
          >
            <Users className="w-3.5 h-3.5" aria-hidden="true" />
            <span>{t("settings.transcribeFiles.speakers.labelAction")}</span>
          </Button>
          <p className="text-xs text-text/60">
            {t("settings.transcribeFiles.speakers.labelHint", {
              file: isolateLtr(transcript.source_name),
            })}
          </p>
        </div>
      )}
    </>
  );

  const empty =
    !labelling && !failure && !noSpeech && !offerModel && !offerLabel;
  if (empty && replace) return null;

  return (
    <div
      ref={sectionRef}
      role="group"
      aria-labelledby={replace ? undefined : titleId}
      aria-label={
        replace ? t("settings.transcribeFiles.speakers.label") : undefined
      }
      className="space-y-2"
      onFocus={() => {
        focusInSectionRef.current = true;
      }}
      onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) {
          focusInSectionRef.current = false;
        }
      }}
    >
      {!replace && (
        <h3
          id={titleId}
          className="text-xs font-medium text-mid-gray uppercase tracking-wide"
        >
          {t("settings.transcribeFiles.speakers.title")}
        </h3>
      )}
      {content}
    </div>
  );
};
