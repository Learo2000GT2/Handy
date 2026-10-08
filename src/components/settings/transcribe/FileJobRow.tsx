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
  errorKey,
  formatDuration,
  isFinishedStatus,
  isolateLtr,
  isRunningStatus,
  statusKey,
  type JobStatusKey,
} from "@/lib/utils/transcript";
import { IconButton } from "../../ui/IconButton";
import { copyToClipboard } from "../history/clipboard";
import { ExportMenu } from "./ExportMenu";

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

const PROGRESS_CLASSES =
  "w-full h-1.5 [&::-webkit-progress-bar]:rounded-full [&::-webkit-progress-bar]:bg-mid-gray/20 [&::-webkit-progress-value]:rounded-full [&::-webkit-progress-value]:bg-logo-primary";

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

  const source: TranscriptSource = { kind: "job", job_id: job.job_id };
  const status = statusKey(job);
  const running = isRunningStatus(job.status);
  const finished = isFinishedStatus(job.status);
  const hasResult = finished && job.has_transcript && !noSpeech;
  const { icon: StatusIcon, className: statusIconClass } = STATUS_ICONS[status];

  const duration =
    job.duration_ms !== null && job.duration_ms > 0 ? job.duration_ms : null;
  let progressText: string | null = null;
  if (running && duration !== null) {
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
          className={`${PROGRESS_CLASSES} ${duration === null ? "animate-pulse" : ""}`}
          max={duration ?? undefined}
          value={
            duration === null ? undefined : Math.min(job.processed_ms, duration)
          }
          aria-label={job.file_name}
          aria-valuetext={progressText ?? undefined}
        />
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
