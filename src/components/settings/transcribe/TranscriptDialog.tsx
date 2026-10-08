import React, { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Copy } from "lucide-react";
import { toast } from "sonner";
import {
  commands,
  type FileTranscript,
  type TranscriptSource,
} from "@/bindings";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import { isRunningStatus, mergeLiveSegments } from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Dialog } from "../../ui/Dialog";
import { copyToClipboard } from "../history/clipboard";
import { ExportMenu } from "./ExportMenu";
import { TranscriptSegments } from "./TranscriptSegments";

interface TranscriptDialogProps {
  /** The transcript to show; null keeps the dialog closed. */
  source: TranscriptSource | null;
  /** Shown in the title until the transcript has loaded. */
  fileName: string;
  onClose: () => void;
}

interface LoadedTranscript {
  key: string;
  transcript: FileTranscript | null;
  error: string | null;
}

const sourceKey = (source: TranscriptSource | null): string | null => {
  if (!source) return null;
  return source.kind === "job"
    ? `job:${source.job_id}`
    : `history:${source.history_id}`;
};

/**
 * Timestamped transcript of a file job or a History entry, with copy and
 * export. While the job runs, new chunks are appended as they arrive.
 */
export const TranscriptDialog: React.FC<TranscriptDialogProps> = ({
  source,
  fileName,
  onClose,
}) => {
  const { t } = useTranslation();
  const jobId = source?.kind === "job" ? source.job_id : null;
  const jobStatus = useFileTranscriptionStore((state) =>
    jobId === null ? undefined : state.jobs[jobId]?.status,
  );
  const liveSegments = useFileTranscriptionStore((state) =>
    jobId === null ? undefined : state.liveSegments[jobId],
  );
  const live = jobStatus !== undefined && isRunningStatus(jobStatus);

  // Results are tagged with the source they belong to, so switching sources
  // never shows the previous transcript.
  const key = sourceKey(source);
  const [loaded, setLoaded] = useState<LoadedTranscript | null>(null);
  const current = loaded !== null && loaded.key === key ? loaded : null;
  const transcript = current?.transcript ?? null;
  const error = current?.error ?? null;

  // Load when opened, and again once a running job finishes so the final
  // transcript replaces the live one.
  useEffect(() => {
    if (!source || key === null) return;

    let cancelled = false;
    const finish = (next: Omit<LoadedTranscript, "key">) => {
      if (cancelled) return;
      setLoaded((previous) => {
        const kept = previous?.key === key ? previous.transcript : null;
        return {
          key,
          transcript: next.transcript ?? kept,
          error: next.error,
        };
      });
    };

    commands
      .getFileTranscript(source)
      .then((result) => {
        if (result.status === "ok") {
          finish({ transcript: result.data, error: null });
        } else {
          // A running job has no transcript until its first chunk is done.
          finish({ transcript: null, error: live ? null : result.error });
        }
      })
      .catch((caught) => {
        finish({
          transcript: null,
          error: caught instanceof Error ? caught.message : String(caught),
        });
      });

    return () => {
      cancelled = true;
    };
    // `key` identifies `source`, whose object may be recreated on each render.
  }, [key, live]);

  // Live segments are kept until the job is removed, so merging them also
  // bridges the moment between a job finishing and its final transcript.
  const segments = useMemo(
    () => mergeLiveSegments(transcript?.segments ?? [], liveSegments ?? []),
    [transcript, liveSegments],
  );
  const loading = source !== null && current === null;

  const hasText =
    !live && transcript !== null && transcript.text.trim().length > 0;

  const handleCopy = async () => {
    if (!transcript) return;
    if (await copyToClipboard(transcript.text)) {
      toast.success(t("settings.transcribeFiles.copied"));
    } else {
      toast.error(t("settings.transcribeFiles.copyError"));
    }
  };

  let body: React.ReactNode;
  if (error !== null) {
    body = (
      <Alert variant="error">
        {t("settings.transcribeFiles.errors.loadTranscript", { error })}
      </Alert>
    );
  } else if (segments.length === 0 && (loading || live)) {
    body = (
      <div
        className="flex justify-center py-8"
        role="status"
        aria-label={t("common.loading")}
      >
        <div className="w-6 h-6 border-2 border-logo-primary border-t-transparent rounded-full animate-spin" />
      </div>
    );
  } else {
    body = (
      <div className="space-y-3">
        {!live && transcript?.status === "partial" && (
          <Alert variant="warning">
            {transcript.status_detail
              ? t("settings.transcribeFiles.transcript.partial", {
                  reason: transcript.status_detail,
                })
              : t("settings.transcribeFiles.transcript.partialNoReason")}
          </Alert>
        )}
        {transcript?.timing === "chunk" && segments.length > 0 && (
          <p className="text-xs text-text/60">
            {t("settings.transcribeFiles.transcript.approximateTimes")}
          </p>
        )}
        {segments.length > 0 ? (
          <TranscriptSegments segments={segments} follow={live} />
        ) : (
          <p className="py-4 text-center text-sm text-text/60">
            {t("settings.transcribeFiles.transcript.empty")}
          </p>
        )}
      </div>
    );
  }

  return (
    <Dialog
      open={source !== null}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={t("settings.transcribeFiles.transcript.title", {
        file: transcript?.source_name ?? fileName,
      })}
      description={
        live ? t("settings.transcribeFiles.transcript.live") : undefined
      }
      closeLabel={t("common.close")}
      className="!max-w-2xl"
      contentFades={false}
      footer={
        source && (
          <>
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="flex items-center gap-1.5"
              onClick={handleCopy}
              disabled={!hasText}
            >
              <Copy className="w-3.5 h-3.5" aria-hidden="true" />
              <span>{t("settings.transcribeFiles.actions.copy")}</span>
            </Button>
            <ExportMenu
              source={source}
              sourceName={transcript?.source_name ?? fileName}
              disabled={!hasText}
              placement="top"
            />
          </>
        )
      }
    >
      {body}
    </Dialog>
  );
};
