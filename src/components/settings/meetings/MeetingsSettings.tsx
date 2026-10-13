import React, { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { useMeetingStore } from "@/stores/meetingStore";
import { LiveMeeting } from "./LiveMeeting";
import { MeetingSetup } from "./MeetingSetup";
import { PastMeetings } from "./PastMeetings";

/** Record an in-person meeting with a live transcript preview. */
export const MeetingsSettings: React.FC = () => {
  const { t } = useTranslation();
  const status = useMeetingStore((state) => state.status);
  const refreshMeetings = useMeetingStore((state) => state.refreshMeetings);
  const clearRecovered = useMeetingStore((state) => state.clearRecovered);

  useEffect(() => {
    void refreshMeetings();
    // Recovered meetings are listed below; the page has now shown them.
    clearRecovered();
  }, [refreshMeetings, clearRecovered]);

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div>
        <h1 className="text-xl font-semibold mb-2">
          {t("settings.meetings.title")}
        </h1>
        <p className="text-sm text-text/60">
          {t("settings.meetings.description")}
        </p>
      </div>
      {status ? <LiveMeeting status={status} /> : <MeetingSetup />}
      <PastMeetings />
    </div>
  );
};
