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
import {
  isFinishedStatus,
  isRunningStatus,
  labelJobFor,
  mergeLiveSegments,
  savedJobFor,
} from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Dialog } from "../../ui/Dialog";
import { copyToClipboard } from "../history/clipboard";
import { ExportMenu } from "./ExportMenu";
import { SavedTranscript } from "./SavedTranscript";
import { SpeakerLabelPrompt } from "./SpeakerLabelPrompt";
import { SpeakerPanel } from "./SpeakerPanel";
import { TranscriptSegments, type TurnRef } from "./TranscriptSegments";

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

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

/**
 * Timestamped transcript of a file job or a History entry, with copy and
 * export. While the job runs, new chunks are appended as they arrive. A
 * finished transcript can be given speaker labels, and labels can be fixed:
 * rename, merge, change the speaker of a passage, undo, label again.
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

  // A speaker-label job for this transcript, queued, running or the last
  // one to finish.
  const historyId = transcript?.history_id ?? null;
  const labelJob = useFileTranscriptionStore((state) =>
    source === null ? null : labelJobFor(state.jobs, source, historyId),
  );
  const labelling = labelJob !== null && !isFinishedStatus(labelJob.status);
  // The file this transcript was saved as automatically, if it was.
  const savedJob = useFileTranscriptionStore((state) =>
    source === null ? null : savedJobFor(state.jobs, source, historyId),
  );
  const labelTranscriptSpeakers = useFileTranscriptionStore(
    (state) => state.labelTranscriptSpeakers,
  );
  const speakerModel = useFileTranscriptionStore((state) => state.speakerModel);

  // A speaker edit is being saved.
  const [saving, setSaving] = useState(false);
  // An edit made since the transcript was (re)loaded can be undone.
  const [canUndo, setCanUndo] = useState(false);
  // A new speaker whose rename form opens at once.
  const [autoRename, setAutoRename] = useState<number | null>(null);
  // Label Again was asked for while the speaker model is missing.
  const [relabelRequested, setRelabelRequested] = useState(false);

  // Load when opened, and again once a running job finishes so the final
  // transcript replaces the live one, and once a label job starts or ends
  // so its labels show.
  useEffect(() => {
    if (!source || key === null) return;
    setCanUndo(false);
    setAutoRename(null);

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
  }, [key, live, labelling]);

  useEffect(() => {
    setRelabelRequested(false);
  }, [key]);

  // Live segments are kept until the job is removed, so merging them also
  // bridges the moment between a job finishing and its final transcript.
  const segments = useMemo(
    () => mergeLiveSegments(transcript?.segments ?? [], liveSegments ?? []),
    [transcript, liveSegments],
  );
  const loading = source !== null && current === null;

  const hasText =
    !live && transcript !== null && transcript.text.trim().length > 0;
  // Labels come with the final transcript; live chunks have none.
  const speakers = live ? null : (transcript?.speakers ?? null);
  // A labelled transcript reads as speaker turns (its paragraphs).
  const turns =
    speakers !== null && transcript !== null && transcript.paragraphs.length > 0
      ? transcript.paragraphs
      : null;

  const showTranscript = (next: FileTranscript) => {
    setLoaded((previous) =>
      previous !== null && previous.key === key
        ? { ...previous, transcript: next }
        : previous,
    );
  };

  // A rename, merge or passage change was saved.
  const handleChanged = (changed: FileTranscript) => {
    showTranscript(changed);
    setCanUndo(true);
  };

  const handleUndo = async () => {
    if (!source) return;
    let error: string | null = null;
    try {
      const result = await commands.undoTranscriptSpeakerEdit(source);
      if (result.status === "ok") {
        showTranscript(result.data);
      } else {
        error = result.error;
      }
    } catch (caught) {
      error = errorText(caught);
    }
    // One level: whether it worked or not, there is nothing left to undo.
    setCanUndo(false);
    if (error !== null) {
      toast.error(t("settings.transcribeFiles.speakers.undoError", { error }));
    }
  };

  const handleLabel = async (replace: boolean, path?: string) => {
    if (!source) return;
    if (replace && path === undefined && speakerModel?.downloaded === false) {
      // Label Again shows the model download instead.
      setRelabelRequested(true);
      return;
    }
    setRelabelRequested(false);
    await labelTranscriptSpeakers(source, replace, path);
  };

  const handleChangeSpeaker = async (
    turn: TurnRef,
    to: number | null,
  ): Promise<boolean> => {
    if (!source || saving) return false;
    const before = new Set(
      transcript?.speakers?.speakers.map((speaker) => speaker.id) ?? [],
    );
    setAutoRename(null);
    setSaving(true);
    let error: string | null = null;
    try {
      const result = await commands.reassignTranscriptTurn(
        source,
        turn.index,
        turn.startMs,
        turn.fromSpeaker,
        to,
      );
      if (result.status === "ok") {
        handleChanged(result.data);
        if (to === null) {
          const added = result.data.speakers?.speakers.find(
            (speaker) => !before.has(speaker.id),
          );
          setAutoRename(added?.id ?? null);
        }
      } else {
        error = result.error;
      }
    } catch (caught) {
      error = errorText(caught);
    } finally {
      setSaving(false);
    }
    if (error !== null) {
      toast.error(
        t("settings.transcribeFiles.speakers.changeSpeakerError", { error }),
      );
      return false;
    }
    return true;
  };

  const handleCopy = async () => {
    if (!transcript) return;
    if (await copyToClipboard(transcript.text)) {
      toast.success(t("settings.transcribeFiles.copied"));
    } else {
      toast.error(t("settings.transcribeFiles.copyError"));
    }
  };

  // Speaker labels can be made or fixed once the transcript is final.
  const canLabel =
    source !== null &&
    transcript !== null &&
    hasText &&
    (transcript.paragraphs.length > 0 || transcript.segments.length > 0);
  let labelSection: React.ReactNode = null;
  if (canLabel && transcript !== null && source !== null) {
    const prompt = (
      <SpeakerLabelPrompt
        transcript={transcript}
        labelJob={labelJob}
        relabelRequested={relabelRequested}
        onLabel={handleLabel}
      />
    );
    labelSection =
      speakers === null ? (
        prompt
      ) : (
        <>
          {prompt}
          <SpeakerPanel
            key={key}
            source={source}
            speakers={speakers}
            onChanged={handleChanged}
            busy={labelling}
            saving={saving}
            onSavingChange={setSaving}
            canUndo={canUndo && !labelling}
            onUndo={handleUndo}
            onRelabel={() => void handleLabel(true)}
            autoRename={autoRename}
          />
        </>
      );
  }

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
        {!live && savedJob !== null && (
          <SavedTranscript job={savedJob} showExportHint={true} />
        )}
        {!live && transcript?.status === "partial" && (
          <Alert variant="warning">
            {transcript.status_detail
              ? t("settings.transcribeFiles.transcript.partial", {
                  reason: transcript.status_detail,
                })
              : t("settings.transcribeFiles.transcript.partialNoReason")}
          </Alert>
        )}
        {labelSection}
        {transcript?.timing === "chunk" && (turns ?? segments).length > 0 && (
          <p className="text-xs text-text/60">
            {t("settings.transcribeFiles.transcript.approximateTimes")}
          </p>
        )}
        {speakers?.approximate && (
          <p className="text-xs text-text/60">
            {t("settings.transcribeFiles.speakers.approximate")}
          </p>
        )}
        {speakers !== null && speakers.unassigned_segments > 0 && (
          <p className="text-xs text-text/60">
            {t("settings.transcribeFiles.speakers.unassigned")}
          </p>
        )}
        {turns !== null && speakers !== null ? (
          <TranscriptSegments
            segments={turns}
            speakers={speakers}
            onChangeSpeaker={handleChangeSpeaker}
            disabled={labelling}
            saving={saving}
          />
        ) : segments.length > 0 ? (
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
