import { create } from "zustand";
import { toast } from "sonner";
import i18n from "@/i18n";
import {
  commands,
  events,
  type MeetingEvent,
  type MeetingStatus,
  type MeetingSummary,
  type MicCheckResult,
  type StartMeetingOptions,
  type TranscriptSegment,
} from "@/bindings";

const LIVE_TRANSCRIPT_STORAGE_KEY = "handy.meetings.liveTranscript";

const readLiveTranscript = (): boolean => {
  try {
    return localStorage.getItem(LIVE_TRANSCRIPT_STORAGE_KEY) !== "false";
  } catch {
    return true;
  }
};

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

export interface CheckLevel {
  rms_db: number;
  peak_db: number;
  elapsed_ms: number;
}

interface MeetingStoreState {
  /** The running meeting; null when none runs. */
  status: MeetingStatus | null;
  /** Live transcript of the running (or last) meeting. */
  segments: TranscriptSegment[];
  /** Past meetings, newest first. */
  meetings: MeetingSummary[];
  /** Meetings closed by recovery at this start (shown once). */
  recovered: string[];
  checkRunning: boolean;
  checkLevel: CheckLevel | null;
  checkResult: MicCheckResult | null;
  checkError: string | null;
  starting: boolean;
  stopping: boolean;
  /** Quitting was held back because a meeting is recording. */
  quitRequested: boolean;
  /** Persisted per device; defaults to on. */
  liveTranscript: boolean;
  initialized: boolean;

  initialize: () => Promise<void>;
  refreshMeetings: () => Promise<void>;
  runCheck: () => Promise<void>;
  cancelCheck: () => Promise<void>;
  start: (options: StartMeetingOptions) => Promise<boolean>;
  setPaused: (paused: boolean) => Promise<void>;
  addNote: (label: string | null) => Promise<boolean>;
  stop: () => Promise<void>;
  stopAndQuit: () => Promise<void>;
  dismissQuit: () => void;
  clearRecovered: () => void;
  transcribe: (id: string) => Promise<void>;
  openFolder: (id: string | null) => Promise<void>;
  setLiveTranscript: (enabled: boolean) => void;
}

export const useMeetingStore = create<MeetingStoreState>()((set, get) => {
  const handleEvent = (event: MeetingEvent) => {
    switch (event.kind) {
      case "status": {
        const previous = get().status;
        // A new meeting starts with an empty live transcript.
        if (event.status && previous?.id !== event.status.id && previous) {
          set({ segments: [] });
        }
        set({ status: event.status });
        break;
      }
      case "segments":
        if (get().status?.id === event.meeting_id || get().status === null) {
          set((state) => ({
            segments: [...state.segments, ...event.segments],
          }));
        }
        break;
      case "check_level":
        set({
          checkLevel: {
            rms_db: event.rms_db,
            peak_db: event.peak_db,
            elapsed_ms: event.elapsed_ms,
          },
        });
        break;
      case "stopped":
        set({ status: null });
        void get().refreshMeetings();
        break;
      case "quit_requested":
        set({ quitRequested: true });
        break;
    }
  };

  return {
    status: null,
    segments: [],
    meetings: [],
    recovered: [],
    checkRunning: false,
    checkLevel: null,
    checkResult: null,
    checkError: null,
    starting: false,
    stopping: false,
    quitRequested: false,
    liveTranscript: readLiveTranscript(),
    initialized: false,

    initialize: async () => {
      if (get().initialized) return;
      set({ initialized: true });
      // Listen before reading the state so no change between the two is missed.
      try {
        await events.meetingEvent.listen((event) => handleEvent(event.payload));
      } catch (error) {
        console.error("Failed to listen for meeting events:", error);
      }
      try {
        const result = await commands.getMeetingState();
        if (result.status === "ok") {
          set({
            status: result.data.status,
            segments: result.data.segments,
            recovered: result.data.recovered,
          });
        } else {
          console.error("Failed to read the meeting state:", result.error);
        }
      } catch (error) {
        console.error("Failed to read the meeting state:", error);
      }
      await get().refreshMeetings();
    },

    refreshMeetings: async () => {
      try {
        const result = await commands.listMeetings();
        if (result.status === "ok") {
          set({ meetings: result.data });
        } else {
          console.error("Failed to list meetings:", result.error);
        }
      } catch (error) {
        console.error("Failed to list meetings:", error);
      }
    },

    runCheck: async () => {
      if (get().checkRunning) return;
      set({
        checkRunning: true,
        checkLevel: null,
        checkResult: null,
        checkError: null,
      });
      try {
        const result = await commands.checkMeetingMicrophone();
        if (result.status === "ok") {
          set({ checkResult: result.data });
        } else {
          set({ checkError: result.error });
        }
      } catch (error) {
        set({ checkError: errorText(error) });
      } finally {
        set({ checkRunning: false, checkLevel: null });
      }
    },

    cancelCheck: async () => {
      await commands.cancelMeetingMicrophoneCheck();
    },

    start: async (options) => {
      if (get().starting) return false;
      set({ starting: true });
      try {
        const result = await commands.startMeeting(options);
        if (result.status === "ok") {
          set({ status: result.data, segments: [] });
          return true;
        }
        toast.error(i18n.t("settings.meetings.errors.startFailed"), {
          description: result.error,
        });
        return false;
      } catch (error) {
        toast.error(i18n.t("settings.meetings.errors.startFailed"), {
          description: errorText(error),
        });
        return false;
      } finally {
        set({ starting: false });
      }
    },

    setPaused: async (paused) => {
      const result = await commands.pauseMeeting(paused);
      if (result.status === "ok") {
        set({ status: result.data });
      } else {
        toast.error(result.error);
      }
    },

    addNote: async (label) => {
      const result = await commands.addMeetingNote(label);
      if (result.status === "ok") {
        toast.success(i18n.t("settings.meetings.live.noteAdded"));
        return true;
      }
      toast.error(result.error);
      return false;
    },

    stop: async () => {
      if (get().stopping) return;
      set({ stopping: true });
      try {
        const result = await commands.stopMeeting();
        if (result.status === "ok") {
          set({ status: null });
          toast.success(i18n.t("settings.meetings.stopped.title"), {
            description:
              result.data.manifest.state === "queued"
                ? i18n.t("settings.meetings.stopped.queued")
                : i18n.t("settings.meetings.stopped.notQueued"),
          });
          await get().refreshMeetings();
        } else {
          toast.error(i18n.t("settings.meetings.errors.stopFailed"), {
            description: result.error,
          });
        }
      } finally {
        set({ stopping: false });
      }
    },

    stopAndQuit: async () => {
      set({ stopping: true });
      const result = await commands.stopMeetingAndQuit();
      if (result.status === "error") {
        set({ stopping: false });
        toast.error(i18n.t("settings.meetings.errors.stopFailed"), {
          description: result.error,
        });
      }
    },

    dismissQuit: () => set({ quitRequested: false }),

    clearRecovered: () => set({ recovered: [] }),

    transcribe: async (id) => {
      const result = await commands.transcribeMeeting(id);
      if (result.status === "ok") {
        toast.success(i18n.t("settings.meetings.past.queued"));
        await get().refreshMeetings();
      } else {
        toast.error(i18n.t("settings.meetings.errors.transcribeFailed"), {
          description: result.error,
        });
      }
    },

    openFolder: async (id) => {
      const result = await commands.openMeetingFolder(id);
      if (result.status === "error") {
        toast.error(result.error);
      }
    },

    setLiveTranscript: (enabled) => {
      set({ liveTranscript: enabled });
      try {
        localStorage.setItem(LIVE_TRANSCRIPT_STORAGE_KEY, String(enabled));
      } catch {
        // Not persisted; the choice still applies for this session.
      }
    },
  };
});
