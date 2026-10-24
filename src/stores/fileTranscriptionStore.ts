import { create } from "zustand";
import { produce } from "immer";
import { toast } from "sonner";
import i18n from "@/i18n";
import {
  commands,
  events,
  type FileJobSnapshot,
  type FileTranscriptionEvent,
  type RecordingsEngineEvent,
  type SpeakerModelEvent,
  type SpeakerModelStatus,
  type SupportedAudioFormats,
  type TranscriptSegment,
  type TranscriptSource,
} from "@/bindings";
import {
  isFinishedStatus,
  isLabelJob,
  isLabelTarget,
  isRunningStatus,
  updateRateEstimate,
  type RateEstimate,
} from "@/lib/utils/transcript";
import { navigateTo } from "@/components/settings/meetings/MeetingChip";
import { showTranscriptReady } from "@/components/settings/transcribe/TranscriptReadyToast";

const SAVE_TO_HISTORY_STORAGE_KEY = "handy.transcribeFiles.saveToHistory";
const LABEL_SPEAKERS_STORAGE_KEY = "handy.transcribeFiles.labelSpeakers";

/** A stored "true"/"false"; anything else reads as `defaultValue`. */
const readFlag = (key: string, defaultValue: boolean): boolean => {
  try {
    const value = localStorage.getItem(key);
    return defaultValue ? value !== "false" : value === "true";
  } catch {
    // localStorage may be unavailable; fall back to the default.
    return defaultValue;
  }
};

const writeFlag = (key: string, value: boolean): void => {
  try {
    localStorage.setItem(key, String(value));
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
  /** Persisted per device in localStorage; defaults to false. */
  labelSpeakers: boolean;
  /** The speaker model on disk; null until first loaded. */
  speakerModel: SpeakerModelStatus | null;
  /** Set while the speaker model downloads. */
  speakerDownload: SpeakerDownload | null;
  /** Why the last speaker model download failed. */
  speakerDownloadError: string | null;
  /** The recordings engine's state (its own model for recordings); null until loaded. */
  recordingsEngine: RecordingsEngineEvent | null;
  /** How fast each running job goes, for its time left. */
  estimates: Record<number, RateEstimate>;
  /** A transcript to show on the Transcribe page (from a toast elsewhere). */
  viewRequest: TranscriptViewRequest | null;
  initialized: boolean;

  initialize: () => Promise<void>;
  refreshFormats: () => Promise<void>;
  enqueue: (paths: string[]) => Promise<FileJobSnapshot[]>;
  cancel: (jobId: number) => Promise<void>;
  cancelAll: () => Promise<void>;
  dismiss: (jobId: number) => Promise<void>;
  /**
   * Dismisses every completed, cancelled or failed job, except those an
   * unfinished speaker-label job is labelling.
   */
  clearFinished: () => Promise<void>;
  /**
   * Queues a speaker-label job for a finished transcript (`replace`: label
   * it again; `path`: a recording found in place of the stored one).
   * Returns the job, or null after showing why it couldn't be queued.
   */
  labelTranscriptSpeakers: (
    source: TranscriptSource,
    replace: boolean,
    path?: string,
  ) => Promise<FileJobSnapshot | null>;
  /** Shows the Transcribe page with this transcript open. */
  requestView: (request: TranscriptViewRequest) => void;
  clearViewRequest: () => void;
  setSaveToHistory: (value: boolean) => void;
  setLabelSpeakers: (value: boolean) => void;
  refreshSpeakerModel: () => Promise<void>;
  /** Resolves once the download has ended (or failed to start). */
  downloadSpeakerModel: () => Promise<void>;
  cancelSpeakerModelDownload: () => Promise<void>;
  deleteSpeakerModel: () => Promise<void>;
}

export interface TranscriptViewRequest {
  source: TranscriptSource;
  fileName: string;
}

export interface SpeakerDownload {
  downloaded: number;
  total: number;
  /** The file is complete and its checksum is being verified. */
  verifying: boolean;
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
  // The user cancelled the running speaker model download, so its error (if
  // the command reports one) is not a failure to show.
  let speakerDownloadCancelled = false;
  // Status replies can arrive out of order; only the newest request's
  // reply is applied, so a stale "downloading" never outlives the download.
  let speakerStatusRequest = 0;

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
        delete state.estimates[jobId];
        state.order = state.order.filter((id) => id !== jobId);
      }),
    );
  };

  // A completed job can still have an empty transcript (every chunk was
  // silent). The snapshot doesn't say, so look at the text once. Announces
  // the transcript when `announce` (the job has just finished).
  const checkCompletedText = async (job: FileJobSnapshot, announce = false) => {
    // A speaker-label job has no transcript of its own.
    if (job.status !== "completed" || isLabelJob(job)) return;
    if (checkedCompletedJobIds.has(job.job_id)) return;
    checkedCompletedJobIds.add(job.job_id);

    // Only non-empty transcripts are saved to History or as a file.
    const saved = job.history_id !== null || job.saved_file !== null;
    let empty = !saved && !job.has_transcript;
    if (!saved && !empty) {
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

    if (!get().jobs[job.job_id]) return;
    if (empty) {
      set(
        produce((state: FileTranscriptionState) => {
          state.noSpeech[job.job_id] = true;
        }),
      );
    } else if (announce) {
      announceReady(get().jobs[job.job_id] ?? job);
    }
  };

  /** "Transcript ready: FILE", with Open in Handy and Show File. */
  const announceReady = (job: FileJobSnapshot) =>
    showTranscriptReady(job, () =>
      get().requestView({
        source: { kind: "job", job_id: job.job_id },
        fileName: job.file_name,
      }),
    );

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
        const previous = get().jobs[job.job_id];
        // Seen running and now finished: tell the user, wherever they are.
        const justFinished =
          previous !== undefined &&
          !isFinishedStatus(previous.status) &&
          isFinishedStatus(job.status);
        const now = Date.now();
        set(
          produce((state: FileTranscriptionState) => {
            if (!state.jobs[job.job_id]) {
              insertJobId(state.order, job.job_id);
            }
            state.jobs[job.job_id] = job;
            if (isRunningStatus(job.status)) {
              state.estimates[job.job_id] = updateRateEstimate(
                state.estimates[job.job_id] ?? null,
                job.processed_ms,
                job.status === "transcribing",
                now,
              );
            } else {
              delete state.estimates[job.job_id];
            }
          }),
        );
        void checkCompletedText(job, justFinished);
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

  const handleSpeakerModelEvent = (event: SpeakerModelEvent) => {
    switch (event.kind) {
      case "progress":
        set({
          speakerDownload: {
            downloaded: event.downloaded,
            total: event.total,
            verifying: false,
          },
          speakerDownloadError: null,
        });
        break;
      case "verifying":
        set((state) => ({
          speakerDownload: {
            downloaded: state.speakerDownload?.total ?? 0,
            total: state.speakerDownload?.total ?? 0,
            verifying: true,
          },
        }));
        break;
      case "completed":
        set({ speakerDownload: null, speakerDownloadError: null });
        void get().refreshSpeakerModel();
        break;
      case "failed":
        set({ speakerDownload: null, speakerDownloadError: event.message });
        void get().refreshSpeakerModel();
        break;
      case "cancelled":
        set({ speakerDownload: null });
        void get().refreshSpeakerModel();
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
    saveToHistory: readFlag(SAVE_TO_HISTORY_STORAGE_KEY, true),
    labelSpeakers: readFlag(LABEL_SPEAKERS_STORAGE_KEY, false),
    speakerModel: null,
    speakerDownload: null,
    speakerDownloadError: null,
    recordingsEngine: null,
    estimates: {},
    viewRequest: null,
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
      try {
        await events.speakerModelEvent.listen((event) =>
          handleSpeakerModelEvent(event.payload),
        );
      } catch (error) {
        console.error("Failed to listen for speaker model events:", error);
      }
      try {
        await events.recordingsEngineEvent.listen((event) =>
          set({ recordingsEngine: event.payload }),
        );
      } catch (error) {
        console.error("Failed to listen for recordings engine events:", error);
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
        get().refreshSpeakerModel(),
        (async () => {
          try {
            const result = await commands.getRecordingsEngineStatus();
            // An event since the listener started is newer than this.
            if (result.status === "ok" && get().recordingsEngine === null) {
              set({ recordingsEngine: result.data });
            }
          } catch (error) {
            console.error("Failed to read the recordings engine state:", error);
          }
        })(),
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
          label_speakers: get().labelSpeakers,
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
        (id) =>
          jobs[id] &&
          isFinishedStatus(jobs[id].status) &&
          !isLabelTarget(jobs, id),
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

    labelTranscriptSpeakers: async (source, replace, path) => {
      let error: string;
      try {
        const result = await commands.labelTranscriptSpeakers(
          source,
          replace,
          path ?? null,
        );
        if (result.status === "ok") {
          addJobs([result.data]);
          return result.data;
        }
        error = result.error;
      } catch (caught) {
        error = errorText(caught);
      }
      toast.error(
        i18n.t("settings.transcribeFiles.speakers.labelError", { error }),
      );
      return null;
    },

    requestView: (request) => {
      set({ viewRequest: request });
      navigateTo("transcribe");
    },

    clearViewRequest: () => set({ viewRequest: null }),

    setSaveToHistory: (value) => {
      writeFlag(SAVE_TO_HISTORY_STORAGE_KEY, value);
      set({ saveToHistory: value });
    },

    setLabelSpeakers: (value) => {
      writeFlag(LABEL_SPEAKERS_STORAGE_KEY, value);
      set({ labelSpeakers: value });
    },

    refreshSpeakerModel: async () => {
      const request = ++speakerStatusRequest;
      try {
        const result = await commands.getSpeakerModelStatus();
        if (request !== speakerStatusRequest) return;
        if (result.status === "error") {
          console.error(
            "Failed to load the speaker model status:",
            result.error,
          );
          return;
        }
        const status = result.data;
        // The backend knows whether a download runs: it may have started
        // before this window loaded, or ended without an event reaching us.
        set((state) => ({
          speakerModel: status,
          speakerDownload: status.downloading
            ? (state.speakerDownload ?? {
                downloaded: 0,
                total: status.size_bytes,
                verifying: false,
              })
            : null,
        }));
      } catch (error) {
        console.error("Failed to load the speaker model status:", error);
      }
    },

    downloadSpeakerModel: async () => {
      if (get().speakerDownload !== null) return;
      speakerDownloadCancelled = false;
      set({
        speakerDownload: {
          downloaded: 0,
          total: get().speakerModel?.size_bytes ?? 0,
          verifying: false,
        },
        speakerDownloadError: null,
      });

      let error: string | null = null;
      try {
        const result = await commands.downloadSpeakerModel();
        if (result.status === "error") error = result.error;
      } catch (caught) {
        error = errorText(caught);
      }
      await get().refreshSpeakerModel();
      // A "failed" event usually reported this already; this also covers
      // failures before the download started. "Already downloading" is no
      // failure: the status then still says a download runs.
      if (
        error !== null &&
        !speakerDownloadCancelled &&
        get().speakerModel?.downloading !== true
      ) {
        set({ speakerDownload: null, speakerDownloadError: error });
      }
    },

    cancelSpeakerModelDownload: async () => {
      speakerDownloadCancelled = true;
      try {
        const result = await commands.cancelSpeakerModelDownload();
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

    deleteSpeakerModel: async () => {
      try {
        const result = await commands.deleteSpeakerModel();
        if (result.status === "error") {
          toast.error(
            i18n.t("settings.transcribeFiles.speakers.removeError", {
              error: result.error,
            }),
          );
        }
      } catch (error) {
        toast.error(
          i18n.t("settings.transcribeFiles.speakers.removeError", {
            error: errorText(error),
          }),
        );
      }
      await get().refreshSpeakerModel();
    },
  };
});
