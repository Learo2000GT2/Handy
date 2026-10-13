import React from "react";
import { useTranslation } from "react-i18next";
import { FolderOpen } from "lucide-react";
import type { FileJobSnapshot, MeetingSummary } from "@/bindings";
import { useMeetingStore } from "@/stores/meetingStore";
import { useFileTranscriptionStore } from "@/stores/fileTranscriptionStore";
import { formatClock, isFinishedStatus } from "@/lib/utils/transcript";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";

const prefix = "settings.meetings.past";

type Progress =
  | { kind: "transcribed" }
  | { kind: "running"; job: FileJobSnapshot }
  | { kind: "not_transcribed" }
  | { kind: "no_audio" };

const progressOf = (
  meeting: MeetingSummary,
  jobs: FileJobSnapshot[],
): Progress => {
  if (meeting.history_id !== null) return { kind: "transcribed" };
  if (!meeting.audio_exists || (meeting.manifest.duration_ms ?? 0) === 0) {
    return { kind: "no_audio" };
  }
  const job = jobs
    .filter((job) => job.path === meeting.audio_path)
    .sort((a, b) => b.job_id - a.job_id)[0];
  if (job && !isFinishedStatus(job.status)) return { kind: "running", job };
  // Saved to History during this session; the list's History link is read
  // on the next refresh.
  if (job?.status === "completed" && job.history_id !== null) {
    return { kind: "transcribed" };
  }
  return { kind: "not_transcribed" };
};

const startedText = (startedAt: number, locale: string) =>
  new Date(startedAt).toLocaleString(locale, {
    dateStyle: "medium",
    timeStyle: "short",
  });

/** Earlier meetings, newest first, with their transcription state. */
export const PastMeetings: React.FC = () => {
  const { t, i18n } = useTranslation();
  const meetings = useMeetingStore((state) => state.meetings);
  const transcribe = useMeetingStore((state) => state.transcribe);
  const openFolder = useMeetingStore((state) => state.openFolder);
  const jobsById = useFileTranscriptionStore((state) => state.jobs);
  const jobs = Object.values(jobsById);

  const interrupted = meetings.filter(
    (meeting) =>
      meeting.manifest.interrupted === true &&
      progressOf(meeting, jobs).kind === "not_transcribed",
  );

  return (
    <div className="space-y-2">
      <div className="px-4 flex items-center justify-between gap-2">
        <h2 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
          {t(`${prefix}.title`)}
        </h2>
        <Button
          variant="ghost"
          size="sm"
          onClick={() => void openFolder(null)}
          className="inline-flex items-center gap-1"
        >
          <FolderOpen className="w-3.5 h-3.5" aria-hidden="true" />
          {t(`${prefix}.openFolder`)}
        </Button>
      </div>
      {interrupted.map((meeting) => (
        <Alert variant="warning" key={`interrupted-${meeting.manifest.id}`}>
          {t(`${prefix}.interrupted`, {
            date: startedText(meeting.manifest.started_at, i18n.language),
            duration: formatClock(meeting.manifest.duration_ms ?? 0),
          })}
        </Alert>
      ))}
      {meetings.length === 0 ? (
        <p className="px-4 text-sm text-mid-gray">{t(`${prefix}.empty`)}</p>
      ) : (
        <div className="bg-background border border-mid-gray/20 rounded-lg">
          <ul className="divide-y divide-mid-gray/20">
            {meetings.map((meeting) => {
              const progress = progressOf(meeting, jobs);
              const notes = (meeting.manifest.markers ?? []).filter(
                (marker) => marker.kind === "note",
              ).length;
              return (
                <li
                  key={meeting.manifest.id}
                  className="px-4 py-3 flex items-center justify-between gap-3"
                >
                  <div className="min-w-0">
                    <p className="text-sm font-medium truncate">
                      {meeting.manifest.title !== ""
                        ? meeting.manifest.title
                        : t(`${prefix}.untitled`)}
                    </p>
                    <p className="text-xs text-mid-gray">
                      {startedText(meeting.manifest.started_at, i18n.language)}
                      {" · "}
                      {formatClock(meeting.manifest.duration_ms ?? 0)}
                      {notes > 0 &&
                        ` · ${t(`${prefix}.notes`, { count: notes })}`}
                      {meeting.manifest.interrupted === true &&
                        ` · ${t(`${prefix}.interruptedBadge`)}`}
                    </p>
                    <p className="text-xs text-mid-gray">
                      {progress.kind === "running"
                        ? t(`${prefix}.state.running`)
                        : t(`${prefix}.state.${progress.kind}`)}
                    </p>
                  </div>
                  <div className="flex items-center gap-2 shrink-0">
                    {progress.kind === "not_transcribed" && (
                      <Button
                        variant="primary"
                        size="sm"
                        onClick={() => void transcribe(meeting.manifest.id)}
                      >
                        {t(`${prefix}.transcribe`)}
                      </Button>
                    )}
                    <Button
                      variant="secondary"
                      size="sm"
                      onClick={() => void openFolder(meeting.manifest.id)}
                      aria-label={t(`${prefix}.openMeetingFolder`)}
                      title={t(`${prefix}.openMeetingFolder`)}
                    >
                      <FolderOpen className="w-3.5 h-3.5" aria-hidden="true" />
                    </Button>
                  </div>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </div>
  );
};
