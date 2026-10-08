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
  formatDuration,
  isFinishedStatus,
  isolateLtr,
  isRunningStatus,
  jobSpeakerView,
  speakerSkipKey,
  statusKey,
  type JobSpeakerView,
  type JobStatusKey,
} from "@/lib/utils/transcript";
import { IconButton } from "../../ui/IconButton";
import { copyToClipboard } from "../history/clipboard";
import { ExportMenu } from "./ExportMenu";
import { PROGRESS_CLASSES } from "./progress";

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
            count: view.speakers,
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
  onView: (job: FileJobSnapshot) => void;
}

export const FileJobRow: React.FC<FileJobRowProps> = ({
  job,
  noSpeech,
  onView,
}) => {
  const { t } = useTranslation();
  const cancel = useFileTranscriptionStore((state) => state.cancel);
  const dismiss = useFileTranscriptionStore((state) => state.dismiss);
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
  const status = statusKey(job);
  const running = isRunningStatus(job.status);
  const finished = isFinishedStatus(job.status);
  const hasResult = finished && job.has_transcript && !noSpeech;
  const { icon: StatusIcon, className: statusIconClass } = STATUS_ICONS[status];

  const duration =
    job.duration_ms !== null && job.duration_ms > 0 ? job.duration_ms : null;
  const speakerWait = awaitingSpeakers(job);
  // While the job only waits for its labels, the progress text says so.
  const speakerView = noSpeech || speakerWait ? null : jobSpeakerView(job);
  let progressText: string | null = null;
  if (speakerWait) {
    progressText = t("settings.transcribeFiles.speakers.state.running");
  } else if (running && duration !== null) {
    progressText = t("settings.transcribeFiles.progress", {
      processed: isolateLtr(formatDuration(job.processed_ms)),
      total: isolateLtr(formatDuration(duration)),
    });
  } else if (running && job.processed_ms > 0) {
    progressText = t("settings.transcribeFiles.progressNoTotal", {
      processed: isolateLtr(formatDuration(job.processed_ms)),
    });
  }

  const handleCopy = async () => {
    try {
      const result = await commands.getFileTranscript(source);
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
            <span>{t(`settings.transcribeFiles.status.${status}`)}</span>
            {progressText && (
              <span className="tabular-nums">{progressText}</span>
            )}
            {speakerView && <SpeakerStatus view={speakerView} />}
            <span className="text-text/50" title={job.model_id ?? undefined}>
              {t(`settings.transcribeFiles.engine.${job.engine}`)}
            </span>
            {job.history_id !== null && (
              <span className="text-text/50">
                {t("settings.transcribeFiles.savedToHistory")}
              </span>
            )}
          </p>
        </div>
        <div className="flex items-center shrink-0">
          {(running || hasResult) && (
            <IconButton
              onClick={() => onView(job)}
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
                source={source}
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
              title={t("settings.transcribeFiles.actions.remove")}
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
        <progress
          className={`${PROGRESS_CLASSES} ${duration === null || speakerWait ? "animate-pulse" : ""}`}
          max={duration ?? undefined}
          value={
            duration === null ? undefined : Math.min(job.processed_ms, duration)
          }
          aria-label={job.file_name}
          aria-valuetext={progressText ?? undefined}
        />
      )}

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
