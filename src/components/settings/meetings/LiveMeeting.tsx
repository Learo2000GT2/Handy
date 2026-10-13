import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { ask } from "@tauri-apps/plugin-dialog";
import { Flag, FolderOpen, Pause, Play, Square } from "lucide-react";
import type { MeetingMarker, MeetingStatus } from "@/bindings";
import { useMeetingStore } from "@/stores/meetingStore";
import { formatClock } from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { TranscriptSegments } from "../transcribe/TranscriptSegments";
import { LevelMeter } from "./LevelMeter";

const prefix = "settings.meetings.live";

const PhaseDot: React.FC<{ status: MeetingStatus }> = ({ status }) => {
  const recording = status.phase === "recording" && status.signal !== "lost";
  return (
    <span className="relative flex h-3 w-3" aria-hidden="true">
      {recording && (
        <span className="absolute inline-flex h-full w-full rounded-full bg-red-500 opacity-60 animate-ping" />
      )}
      <span
        className={`relative inline-flex h-3 w-3 rounded-full ${
          status.phase === "failed" || status.signal === "lost"
            ? "bg-yellow-500"
            : status.phase === "paused"
              ? "bg-mid-gray"
              : "bg-red-500"
        }`}
      />
    </span>
  );
};

const MarkerLine: React.FC<{ marker: MeetingMarker }> = ({ marker }) => {
  const { t } = useTranslation();
  return (
    <li className="flex gap-2 text-xs">
      <span className="tabular-nums text-mid-gray">
        {formatClock(marker.at_ms)}
      </span>
      <span>
        {t(`${prefix}.markers.${marker.kind}`)}
        {marker.label && marker.kind === "note" ? `: ${marker.label}` : ""}
      </span>
    </li>
  );
};

/** The running meeting: status, level, controls, notes and live transcript. */
export const LiveMeeting: React.FC<{ status: MeetingStatus }> = ({
  status,
}) => {
  const { t } = useTranslation();
  const segments = useMeetingStore((state) => state.segments);
  const setPaused = useMeetingStore((state) => state.setPaused);
  const addNote = useMeetingStore((state) => state.addNote);
  const stop = useMeetingStore((state) => state.stop);
  const stopping = useMeetingStore((state) => state.stopping);
  const openFolder = useMeetingStore((state) => state.openFolder);
  const [note, setNote] = useState("");
  const [savingNote, setSavingNote] = useState(false);

  const paused = status.phase === "paused";
  const busy = stopping || status.phase === "stopping";

  const phaseText =
    status.phase === "failed"
      ? t(`${prefix}.phase.failed`)
      : status.phase === "stopping" || stopping
        ? t(`${prefix}.phase.stopping`)
        : paused
          ? t(`${prefix}.phase.paused`)
          : t(`${prefix}.phase.recording`);

  const handleStop = async () => {
    const confirmed = await ask(t(`${prefix}.stopConfirm`), {
      title: t(`${prefix}.stop`),
      kind: "warning",
    });
    if (confirmed) await stop();
  };

  const handleNote = async () => {
    if (savingNote) return;
    setSavingNote(true);
    try {
      if (await addNote(note.trim() === "" ? null : note)) setNote("");
    } finally {
      setSavingNote(false);
    }
  };

  const liveLine = (() => {
    switch (status.live.kind) {
      case "off":
        return t(`${prefix}.transcript.off`);
      case "starting":
        return t(`${prefix}.transcript.starting`);
      case "unavailable":
        return t(`${prefix}.transcript.unavailable`, {
          message: status.live.message,
        });
      case "listening":
        return segments.length === 0 ? t(`${prefix}.transcript.waiting`) : null;
    }
  })();

  return (
    <div className="space-y-4">
      <div className="rounded-lg border border-mid-gray/20 px-4 py-4 flex flex-col gap-3">
        <div className="flex items-center justify-between gap-3">
          <div className="flex items-center gap-3 min-w-0">
            <PhaseDot status={status} />
            <div className="min-w-0">
              <p className="text-sm font-semibold" role="status">
                {phaseText}
              </p>
              {status.title !== "" && (
                <p className="text-xs text-mid-gray truncate">{status.title}</p>
              )}
            </div>
          </div>
          <span
            className="text-3xl font-semibold tabular-nums"
            aria-label={t(`${prefix}.recordedFor`, {
              time: formatClock(status.recorded_ms),
            })}
          >
            {formatClock(status.recorded_ms)}
          </span>
        </div>

        <LevelMeter
          rmsDb={status.rms_db}
          peakDb={status.peak_db}
          inactive={paused || status.signal === "lost"}
          label={t(`${prefix}.level`)}
        />
        <p className="text-xs text-mid-gray" aria-live="polite">
          {t(`${prefix}.signal.${status.signal}`)}
          {" · "}
          {status.microphone ?? t(`${prefix}.defaultMicrophone`)}
        </p>

        {status.signal === "lost" && (
          <Alert variant="warning">{t(`${prefix}.alerts.micLost`)}</Alert>
        )}
        {status.signal === "no_signal" && (
          <Alert variant="warning">{t(`${prefix}.alerts.noSignal`)}</Alert>
        )}
        {status.write_error !== null && (
          <Alert variant="error">
            {t(`${prefix}.alerts.writeFailed`, { error: status.write_error })}
          </Alert>
        )}
        {status.low_disk && status.write_error === null && (
          <Alert variant="warning">{t(`${prefix}.alerts.lowDisk`)}</Alert>
        )}
        {status.dropped_ms > 0 && (
          <Alert variant="warning">
            {t(`${prefix}.alerts.dropped`, {
              seconds: Math.ceil(status.dropped_ms / 1000),
            })}
          </Alert>
        )}

        <div className="flex flex-wrap items-center gap-2">
          <Button
            variant="secondary"
            size="md"
            onClick={() => void setPaused(!paused)}
            disabled={busy}
            className="inline-flex items-center gap-2"
          >
            {paused ? (
              <Play className="w-4 h-4" aria-hidden="true" />
            ) : (
              <Pause className="w-4 h-4" aria-hidden="true" />
            )}
            {paused ? t(`${prefix}.resume`) : t(`${prefix}.pause`)}
          </Button>
          <Button
            variant="danger"
            size="md"
            onClick={() => void handleStop()}
            disabled={busy}
            className="inline-flex items-center gap-2"
          >
            <Square className="w-4 h-4" aria-hidden="true" />
            {busy ? t(`${prefix}.stopping`) : t(`${prefix}.stop`)}
          </Button>
          <Button
            variant="ghost"
            size="md"
            onClick={() => void openFolder(status.id)}
            className="inline-flex items-center gap-2 ms-auto"
          >
            <FolderOpen className="w-4 h-4" aria-hidden="true" />
            {t(`${prefix}.openFolder`)}
          </Button>
        </div>

        <div className="flex items-center gap-2">
          <Input
            type="text"
            value={note}
            maxLength={200}
            onChange={(e) => setNote(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void handleNote();
              }
            }}
            placeholder={t(`${prefix}.notePlaceholder`)}
            aria-label={t(`${prefix}.notePlaceholder`)}
            variant="compact"
            className="flex-1"
            disabled={busy}
          />
          <Button
            variant="secondary"
            size="md"
            onClick={() => void handleNote()}
            disabled={busy || savingNote}
            className="inline-flex items-center gap-2 shrink-0"
          >
            <Flag className="w-4 h-4" aria-hidden="true" />
            {t(`${prefix}.addNote`)}
          </Button>
        </div>
        {status.markers.length > 0 && (
          <ul
            className="flex flex-col gap-0.5"
            aria-label={t(`${prefix}.markers.title`)}
          >
            {status.markers.map((marker, index) => (
              <MarkerLine key={`${marker.wall_ms}-${index}`} marker={marker} />
            ))}
          </ul>
        )}
      </div>

      <div className="space-y-2">
        <div className="px-4">
          <h2 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
            {t(`${prefix}.transcript.title`)}
          </h2>
          <p className="text-xs text-mid-gray mt-1">
            {t(`${prefix}.transcript.preview`)}
          </p>
        </div>
        <div className="rounded-lg border border-mid-gray/20 px-4 py-3 flex flex-col gap-2">
          {liveLine && (
            <p className="text-sm text-mid-gray" role="status">
              {liveLine}
            </p>
          )}
          {status.live_skipped_ms > 0 && (
            <p className="text-xs text-mid-gray">
              {t(`${prefix}.transcript.behind`)}
            </p>
          )}
          {segments.length > 0 && (
            <TranscriptSegments segments={segments} follow={true} />
          )}
        </div>
      </div>
    </div>
  );
};
