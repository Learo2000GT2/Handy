import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import {
  Ban,
  Check,
  CircleAlert,
  CircleCheck,
  CircleX,
  Clock,
  Copy,
  Eye,
  FolderOpen,
  Loader2,
  Pause,
  Trash2,
  Users,
  X,
} from "lucide-react";
import { toast } from "sonner";
import {
  commands,
  type FileJobSnapshot,
  type TranscriptSource,
} from "@/bindings";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import {
  awaitingSpeakers,
  errorKey,
  estimateRemainingMs,
  formatClock,
  formatDuration,
  formatPercent,
  isFinishedStatus,
  isLabelJob,
  isLabelTarget,
  isolateLtr,
  isRunningStatus,
  jobElapsedMs,
  jobProgressPercent,
  jobSpeakerView,
  labelJobHasResult,
  speakerSkipKey,
  statusKey,
  timeLeftText,
  type JobSpeakerView,
  type JobStatusKey,
} from "@/lib/utils/transcript";
import { useNow } from "@/hooks/useNow";
import { IconButton } from "../../ui/IconButton";
import { copyToClipboard } from "../history/clipboard";
import { ExportMenu } from "./ExportMenu";
import { JOB_PROGRESS_CLASSES } from "./progress";
import { SavedTranscript } from "./SavedTranscript";

const STATUS_ICONS: Record<
  JobStatusKey,
  { icon: React.ElementType; className: string }
> = {
  queued: { icon: Clock, className: "text-text/40" },
  preparing: { icon: Loader2, className: "text-logo-primary animate-spin" },
  waiting: { icon: Pause, className: "text-text/50" },
  transcribing: { icon: Loader2, className: "text-logo-primary animate-spin" },
  completed: { icon: CircleCheck, className: "text-green-500" },
  partial: { icon: CircleAlert, className: "text-yellow-500" },
  cancelled: { icon: Ban, className: "text-text/40" },
  failed: { icon: CircleX, className: "text-red-500" },
};

// A Lemonade job still on its first chunk after this long is probably
// waiting for the server to download or load the model.
const FIRST_CHUNK_SLOW_MS = 15_000;

const SpeakerStatus: React.FC<{ view: JobSpeakerView }> = ({ view }) => {
  const { t } = useTranslation();
  switch (view.kind) {
    case "running":
      return (
        <span className="flex items-center gap-1">
          <Loader2 className="w-3 h-3 animate-spin" aria-hidden="true" />
          {t("settings.transcribeFiles.speakers.state.running")}
        </span>
      );
    case "done":
      return (
        <span className="flex items-center gap-1">
          <Users className="w-3 h-3" aria-hidden="true" />
          {t("settings.transcribeFiles.speakers.state.done", {
            number: view.speakers,
          })}
        </span>
      );
    case "failed":
      return (
        <span className="text-yellow-500" title={view.message}>
          {t("settings.transcribeFiles.speakers.state.failed")}
        </span>
      );
    case "skipped":
      return (
        <span>
          {t(
            `settings.transcribeFiles.speakers.state.skipped.${speakerSkipKey(view.reason)}`,
          )}
        </span>
      );
  }
};

interface FileJobRowProps {
  job: FileJobSnapshot;
  /** Completed, but every part of the file was silent. */
  noSpeech: boolean;
  /** Opens the transcript `source` (a label job's: the one it labelled). */
  onView: (source: TranscriptSource, fileName: string) => void;
}

export const FileJobRow: React.FC<FileJobRowProps> = ({
  job,
  noSpeech,
  onView,
}) => {
  const { t, i18n } = useTranslation();
  const cancel = useFileTranscriptionStore((state) => state.cancel);
  const dismiss = useFileTranscriptionStore((state) => state.dismiss);
  const estimate = useFileTranscriptionStore(
    (state) => state.estimates[job.job_id],
  );
  // A label row's transcript can be gone: the job it labelled may have been
  // removed or evicted since.
  const labelResult = useFileTranscriptionStore((state) =>
    labelJobHasResult(state.jobs, job),
  );
  // The backend keeps a job that an unfinished label job is labelling.
  const removeLocked = useFileTranscriptionStore((state) =>
    isLabelTarget(state.jobs, job.job_id),
  );
  const [showCopied, setShowCopied] = useState(false);

  useEffect(() => {
    if (!showCopied) return;
    const timer = setTimeout(() => setShowCopied(false), 2000);
    return () => clearTimeout(timer);
  }, [showCopied]);

  // Lemonade loads (or first downloads) the model on the first request.
  const onFirstChunk =
    job.engine === "lemonade" &&
    job.status === "transcribing" &&
    job.chunks_done === 0;
  const [firstChunkSlow, setFirstChunkSlow] = useState(false);
  useEffect(() => {
    if (!onFirstChunk) return;
    const timer = setTimeout(
      () => setFirstChunkSlow(true),
      FIRST_CHUNK_SLOW_MS,
    );
    return () => {
      clearTimeout(timer);
      setFirstChunkSlow(false);
    };
  }, [onFirstChunk]);

  const source: TranscriptSource = { kind: "job", job_id: job.job_id };
  // A speaker-label job's row acts on the transcript it labelled.
  const labelJob = isLabelJob(job);
  const viewSource = job.labels_for ?? source;
  const status = statusKey(job);
  const running = isRunningStatus(job.status);
  const finished = isFinishedStatus(job.status);
  const hasResult = labelJob
    ? labelResult
    : finished && job.has_transcript && !noSpeech;
  const { icon: StatusIcon, className: statusIconClass } = STATUS_ICONS[status];

  const duration =
    job.duration_ms !== null && job.duration_ms > 0 ? job.duration_ms : null;
  const speakerWait = !labelJob && awaitingSpeakers(job);
  // While the job only waits for its labels (or only makes labels), the
  // status says so.
  const speakerView =
    noSpeech || speakerWait || (labelJob && running)
      ? null
      : jobSpeakerView(job);
  let statusText = t(`settings.transcribeFiles.status.${status}`);
  if ((labelJob || speakerWait) && running) {
    statusText =
      job.status === "waiting"
        ? t("settings.transcribeFiles.status.waiting")
        : t("settings.transcribeFiles.speakers.state.running");
  }

  // Where the running job is: the diarizer's position while speakers are
  // identified (once it reports one), else the audio transcribed so far.
  const position =
    !running || job.status === "preparing"
      ? null
      : labelJob
        ? job.processed_ms > 0
          ? job.processed_ms
          : null
        : speakerWait
          ? job.speakers_processed_ms
          : job.processed_ms;
  let progressText: string | null = null;
  if (position !== null && duration !== null) {
    progressText = t("settings.transcribeFiles.progress", {
      processed: isolateLtr(formatDuration(Math.min(position, duration))),
      total: isolateLtr(formatDuration(duration)),
    });
  } else if (position !== null && position > 0) {
    progressText = t("settings.transcribeFiles.progressNoTotal", {
      processed: isolateLtr(formatDuration(position)),
    });
  }
  const percent = running ? jobProgressPercent(job) : null;
  const percentText =
    percent === null ? null : formatPercent(percent, i18n.language);

  // Elapsed time ticks every second while the job runs.
  const now = useNow(running);
  const elapsed = jobElapsedMs(job.started_at, job.finished_at, now);
  const elapsedText =
    running && elapsed !== null
      ? t("settings.transcribeFiles.elapsed", {
          time: isolateLtr(formatClock(elapsed)),
        })
      : null;
  const tookText =
    finished && !labelJob && job.finished_at !== null && elapsed !== null
      ? t("settings.transcribeFiles.took", {
          time: isolateLtr(formatClock(elapsed)),
        })
      : null;
  // The time left, only while transcribing (not paused for dictation, not
  // identifying speakers) and once there is enough to estimate from.
  const remaining =
    job.status === "transcribing" && !speakerWait
      ? estimateRemainingMs(estimate, duration, now)
      : null;
  let timeLeft: string | null = null;
  if (remaining !== null) {
    const { key, values } = timeLeftText(remaining);
    timeLeft = t(`settings.transcribeFiles.timeLeft.${key}`, values);
  }

  const handleCopy = async () => {
    try {
      const result = await commands.getFileTranscript(viewSource);
      if (result.status === "error") {
        toast.error(
          t("settings.transcribeFiles.errors.loadTranscript", {
            error: result.error,
          }),
        );
        return;
      }
      if (await copyToClipboard(result.data.text)) {
        setShowCopied(true);
      } else {
        toast.error(t("settings.transcribeFiles.copyError"));
      }
    } catch (error) {
      toast.error(
        t("settings.transcribeFiles.errors.loadTranscript", {
          error: error instanceof Error ? error.message : String(error),
        }),
      );
    }
  };

  const handleShowFile = async () => {
    try {
      await revealItemInDir(job.path);
    } catch (error) {
      console.error("Failed to reveal file:", error);
      toast.error(t("settings.transcribeFiles.errors.notFound"));
    }
  };

  return (
    <div className="px-4 py-3 flex flex-col gap-2">
      <div className="flex items-center gap-3">
        <StatusIcon
          className={`w-5 h-5 shrink-0 ${statusIconClass}`}
          aria-hidden="true"
        />
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium truncate" title={job.path}>
            {job.file_name}
          </p>
          <p className="text-xs text-text/60 flex flex-wrap gap-x-2">
            <span>{statusText}</span>
            {tookText && <span className="tabular-nums">{tookText}</span>}
            {speakerView && <SpeakerStatus view={speakerView} />}
            {labelJob ? (
              <span className="text-text/50">
                {t("settings.transcribeFiles.speakers.jobKind")}
              </span>
            ) : (
              <span className="text-text/50" title={job.model_id ?? undefined}>
                {t(`settings.transcribeFiles.engine.${job.engine}`)}
              </span>
            )}
            {!labelJob && job.history_id !== null && (
              <span className="text-text/50">
                {t("settings.transcribeFiles.savedToHistory")}
              </span>
            )}
          </p>
        </div>
        <div className="flex items-center shrink-0">
          {((running && !labelJob) || hasResult) && (
            <IconButton
              onClick={() => onView(viewSource, job.file_name)}
              title={t("settings.transcribeFiles.actions.view")}
            >
              <Eye width={16} height={16} />
            </IconButton>
          )}
          {hasResult && (
            <>
              <IconButton
                onClick={handleCopy}
                title={t("settings.transcribeFiles.actions.copy")}
              >
                {showCopied ? (
                  <Check width={16} height={16} />
                ) : (
                  <Copy width={16} height={16} />
                )}
              </IconButton>
              <ExportMenu
                source={viewSource}
                sourceName={job.file_name}
                variant="icon"
              />
            </>
          )}
          {job.error?.code !== "not_found" && (
            <IconButton
              onClick={handleShowFile}
              title={t("settings.transcribeFiles.actions.showFile")}
            >
              <FolderOpen width={16} height={16} />
            </IconButton>
          )}
          {finished ? (
            <IconButton
              onClick={() => dismiss(job.job_id)}
              disabled={removeLocked}
              title={
                removeLocked
                  ? t("settings.transcribeFiles.actions.removeWhileLabelling")
                  : t("settings.transcribeFiles.actions.remove")
              }
              aria-label={t("settings.transcribeFiles.actions.remove")}
            >
              <Trash2 width={16} height={16} />
            </IconButton>
          ) : (
            <IconButton
              onClick={() => cancel(job.job_id)}
              title={t("settings.transcribeFiles.actions.cancel")}
            >
              <X width={16} height={16} />
            </IconButton>
          )}
        </div>
      </div>

      {running && (
        <div className="flex flex-col gap-1.5">
          <progress
            className={`${JOB_PROGRESS_CLASSES} ${duration === null || position === null ? "animate-pulse" : ""}`}
            max={duration ?? undefined}
            value={
              duration === null || position === null
                ? undefined
                : Math.min(position, duration)
            }
            aria-label={job.file_name}
            aria-valuetext={
              [percentText, progressText].filter(Boolean).join(", ") ||
              undefined
            }
          />
          <p className="text-xs text-text/70 flex flex-wrap items-baseline gap-x-3 gap-y-0.5 tabular-nums">
            {percentText && (
              <span className="text-sm font-semibold text-text">
                {percentText}
              </span>
            )}
            {progressText && <span>{progressText}</span>}
            {elapsedText && <span>{elapsedText}</span>}
            {timeLeft && <span>{timeLeft}</span>}
          </p>
        </div>
      )}

      {finished && !labelJob && <SavedTranscript job={job} />}

      {onFirstChunk && firstChunkSlow && (
        <p className="text-xs text-text/60">
          {t("settings.transcribeFiles.lemonade.firstChunkSlow")}
        </p>
      )}

      {job.status === "failed" && job.error && (
        <p className="text-xs text-red-400" title={job.error.message}>
          {t(`settings.transcribeFiles.errors.${errorKey(job.error.code)}`)}
        </p>
      )}

      {noSpeech && (
        <p className="text-xs text-text/60">
          {t("settings.transcribeFiles.noSpeech")}
        </p>
      )}
    </div>
  );
};
