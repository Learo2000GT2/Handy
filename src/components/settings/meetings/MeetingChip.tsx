import React from "react";
import { useTranslation } from "react-i18next";
import { useMeetingStore } from "@/stores/meetingStore";
import { formatClock } from "@/lib/utils/transcript";

// Opens a section of the main window (App.tsx listens).
export const NAVIGATE_EVENT = "handy:navigate";

export const navigateTo = (section: string) =>
  window.dispatchEvent(
    new CustomEvent<string>(NAVIGATE_EVENT, { detail: section }),
  );

/** Footer indicator of a recording meeting, on every page. */
export const MeetingChip: React.FC = () => {
  const { t } = useTranslation();
  const status = useMeetingStore((state) => state.status);
  if (!status) return null;
  const warning = status.signal === "lost" || status.phase === "failed";
  return (
    <button
      type="button"
      onClick={() => navigateTo("meetings")}
      className={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 cursor-pointer hover:bg-mid-gray/10 ${warning ? "border-yellow-500 text-yellow-500" : "border-red-500/60 text-red-500"}`}
      title={t("settings.meetings.chip.open")}
    >
      <span
        aria-hidden="true"
        className={`h-2 w-2 rounded-full ${status.phase === "paused" ? "bg-mid-gray" : warning ? "bg-yellow-500" : "bg-red-500 animate-pulse"}`}
      />
      <span>
        {status.phase === "paused"
          ? t("settings.meetings.chip.paused")
          : t("settings.meetings.chip.recording")}
      </span>
      <span className="tabular-nums">{formatClock(status.recorded_ms)}</span>
    </button>
  );
};
