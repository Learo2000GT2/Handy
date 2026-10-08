import { create } from "zustand";
import { produce } from "immer";
import { toast } from "sonner";
import i18n from "@/i18n";
import {
  commands,
  events,
  type FileJobSnapshot,
  type FileTranscriptionEvent,
  type SupportedAudioFormats,
  type TranscriptSegment,
} from "@/bindings";
import { isFinishedStatus } from "@/lib/utils/transcript";

const SAVE_TO_HISTORY_STORAGE_KEY = "handy.transcribeFiles.saveToHistory";

const readSaveToHistory = (): boolean => {
  try {
    return localStorage.getItem(SAVE_TO_HISTORY_STORAGE_KEY) !== "false";
  } catch {
    // localStorage may be unavailable; fall back to the default.
    return true;
  }
};

const writeSaveToHistory = (value: boolean): void => {
  try {
    localStorage.setItem(SAVE_TO_HISTORY_STORAGE_KEY, String(value));
  } catch {
    // Not persisted; the choice still applies for this session.
  }
};

const errorText = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

// Using Record instead of Map for Immer compatibility
interface FileTranscriptionState {
  jobs: Record<number, FileJobSnapshot>;
  /** Job ids in enqueue order (job ids are assigned in that order). */
  order: number[];
  /** Segments from "chunk" events, kept until the job is removed. */
  liveSegments: Record<number, TranscriptSegment[]>;
  /** Completed jobs that found no speech (their transcript text is empty). */
  noSpeech: Record<number, true>;
  formats: SupportedAudioFormats | null;
  /** Persisted per device in localStorage; defaults to true. */
  saveToHistory: boolean;
  initialized: boolean;

  initialize: () => Promise<void>;
  refreshFormats: () => Promise<void>;
  enqueue: (paths: string[]) => Promise<FileJobSnapshot[]>;
  cancel: (jobId: number) => Promise<void>;
  cancelAll: () => Promise<void>;
  dismiss: (jobId: number) => Promise<void>;
  /** Dismisses every completed, cancelled or failed job. */
  clearFinished: () => Promise<void>;
  setSaveToHistory: (value: boolean) => void;
}

export const useFileTranscriptionStore = create<FileTranscriptionState>()((
  set,
  get,
) => {
  // Jobs the backend has removed. A list or enqueue response computed before
  // the removal must not bring them back.
  const removedJobIds = new Set<number>();
  // Completed jobs whose transcript text has been checked for speech.
  const checkedCompletedJobIds = new Set<number>();

  const insertJobId = (order: number[], jobId: number) => {
    // Job ids grow with every enqueue, so sorting by id keeps enqueue order
    // even when events for different jobs arrive out of order.
    const index = order.findIndex((id) => id > jobId);
    if (index === -1) order.push(jobId);
    else order.splice(index, 0, jobId);
  };

  const deleteJob = (jobId: number) => {
    removedJobIds.add(jobId);
    checkedCompletedJobIds.delete(jobId);
    set(
      produce((state: FileTranscriptionState) => {
        delete state.jobs[jobId];
        delete state.liveSegments[jobId];
        delete state.noSpeech[jobId];
        state.order = state.order.filter((id) => id !== jobId);
      }),
    );
  };

  // A completed job can still have an empty transcript (every chunk was
  // silent). The snapshot doesn't say, so look at the text once.
  const checkCompletedText = async (job: FileJobSnapshot) => {
    if (job.status !== "completed") return;
    if (checkedCompletedJobIds.has(job.job_id)) return;
    checkedCompletedJobIds.add(job.job_id);

    // Only non-empty transcripts are saved to History.
    if (job.history_id !== null) return;

    let empty = !job.has_transcript;
    if (!empty) {
      try {
        const result = await commands.getFileTranscript({
          kind: "job",
          job_id: job.job_id,
        });
        if (result.status !== "ok") return;
        empty = result.data.text.trim() === "";
      } catch (error) {
        console.error("Failed to check file transcript:", error);
        return;
      }
    }

    if (empty && get().jobs[job.job_id]) {
      set(
        produce((state: FileTranscriptionState) => {
          state.noSpeech[job.job_id] = true;
        }),
      );
    }
  };

  /**
   * Adds jobs from a command response. Events are the newer source of
   * truth, so a job the store already knows keeps its state.
   */
  const addJobs = (snapshots: FileJobSnapshot[]) => {
    const added = snapshots.filter(
      (job) => !removedJobIds.has(job.job_id) && !get().jobs[job.job_id],
    );
    if (added.length === 0) return;

    set(
      produce((state: FileTranscriptionState) => {
        for (const job of added) {
          if (state.jobs[job.job_id]) continue;
          state.jobs[job.job_id] = job;
          insertJobId(state.order, job.job_id);
        }
      }),
    );
    added.forEach((job) => void checkCompletedText(job));
  };

  const handleEvent = (event: FileTranscriptionEvent) => {
    switch (event.kind) {
      case "updated": {
        const { job } = event;
        if (removedJobIds.has(job.job_id)) return;
        set(
          produce((state: FileTranscriptionState) => {
            if (!state.jobs[job.job_id]) {
              insertJobId(state.order, job.job_id);
            }
            state.jobs[job.job_id] = job;
          }),
        );
        void checkCompletedText(job);
        break;
      }
      case "chunk": {
        const { job_id: jobId, segments } = event;
        if (removedJobIds.has(jobId) || segments.length === 0) return;
        set(
          produce((state: FileTranscriptionState) => {
            const live = state.liveSegments[jobId] ?? [];
            const lastStart =
              live.length > 0 ? live[live.length - 1].start_ms : -1;
            for (const segment of segments) {
              if (segment.start_ms > lastStart) live.push(segment);
            }
            state.liveSegments[jobId] = live;
          }),
        );
        break;
      }
      case "removed":
        deleteJob(event.job_id);
        break;
    }
  };

  /** Returns the error text, or null on success. */
  const dismissJob = async (jobId: number): Promise<string | null> => {
    try {
      const result = await commands.dismissFileTranscriptionJob(jobId);
      if (result.status === "error") return result.error;
      // The "removed" event does the same; this keeps the list in sync
      // even when the event arrives late.
      deleteJob(jobId);
      return null;
    } catch (error) {
      return errorText(error);
    }
  };

  return {
    jobs: {},
    order: [],
    liveSegments: {},
    noSpeech: {},
    formats: null,
    saveToHistory: readSaveToHistory(),
    initialized: false,

    initialize: async () => {
      if (get().initialized) return;
      set({ initialized: true });

      // Listen before listing so no change between the two is missed.
      try {
        await events.fileTranscriptionEvent.listen((event) =>
          handleEvent(event.payload),
        );
      } catch (error) {
        console.error("Failed to listen for file transcription events:", error);
      }

      await Promise.all([
        (async () => {
          try {
            const result = await commands.listFileTranscriptionJobs();
            if (result.status === "ok") {
              addJobs(result.data);
            } else {
              console.error(
                "Failed to list file transcription jobs:",
                result.error,
              );
            }
          } catch (error) {
            console.error("Failed to list file transcription jobs:", error);
          }
        })(),
        get().refreshFormats(),
      ]);
    },

    refreshFormats: async () => {
      try {
        const result = await commands.getSupportedAudioFormats();
        if (result.status === "ok") {
          set({ formats: result.data });
        } else {
          console.error("Failed to load supported formats:", result.error);
        }
      } catch (error) {
        console.error("Failed to load supported formats:", error);
      }
    },

    enqueue: async (paths) => {
      if (paths.length === 0) return [];
      try {
        const result = await commands.enqueueFileTranscriptions(paths, {
          save_to_history: get().saveToHistory,
        });
        if (result.status === "ok") {
          addJobs(result.data);
          return result.data;
        }
        toast.error(
          i18n.t("settings.transcribeFiles.errors.enqueue", {
            error: result.error,
          }),
        );
      } catch (error) {
        toast.error(
          i18n.t("settings.transcribeFiles.errors.enqueue", {
            error: errorText(error),
          }),
        );
      }
      return [];
    },

    cancel: async (jobId) => {
      let error: string | null = null;
      try {
        const result = await commands.cancelFileTranscription(jobId);
        if (result.status === "error") error = result.error;
      } catch (caught) {
        error = errorText(caught);
      }
      // A job that finished or went away in the meantime needs no toast.
      const job = get().jobs[jobId];
      if (error !== null && job && !isFinishedStatus(job.status)) {
        toast.error(
          i18n.t("settings.transcribeFiles.errors.cancel", { error }),
        );
      }
    },

    cancelAll: async () => {
      try {
        const result = await commands.cancelAllFileTranscriptions();
        if (result.status === "error") {
          toast.error(
            i18n.t("settings.transcribeFiles.errors.cancel", {
              error: result.error,
            }),
          );
        }
      } catch (error) {
        toast.error(
          i18n.t("settings.transcribeFiles.errors.cancel", {
            error: errorText(error),
          }),
        );
      }
    },

    dismiss: async (jobId) => {
      const error = await dismissJob(jobId);
      if (error !== null) {
        toast.error(
          i18n.t("settings.transcribeFiles.errors.remove", { error }),
        );
      }
    },

    clearFinished: async () => {
      const { jobs, order } = get();
      const finished = order.filter(
        (id) => jobs[id] && isFinishedStatus(jobs[id].status),
      );
      const errors = await Promise.all(finished.map(dismissJob));
      const firstError = errors.find((error) => error !== null);
      if (firstError) {
        toast.error(
          i18n.t("settings.transcribeFiles.errors.remove", {
            error: firstError,
          }),
        );
      }
    },

    setSaveToHistory: (value) => {
      writeSaveToHistory(value);
      set({ saveToHistory: value });
    },
  };
});
