/**
 * Pure helpers for the "Transcribe Files" page and file entries in History.
 *
 * Nothing here touches Tauri or i18n so the module can be tested with plain
 * `bun` (see transcript.test.ts). Export rendering (TXT, Markdown, SRT, VTT)
 * lives in Rust; these helpers only cover display and file-name concerns.
 */
import type {
  FileJobErrorCode,
  FileJobSnapshot,
  FileJobStatus,
  SupportedAudioFormats,
  TranscriptFormat,
  TranscriptSegment,
} from "@/bindings";

/** Formats listed on the drop zone. The full list comes from the backend. */
export const DISPLAY_FORMATS = [
  "WAV",
  "MP3",
  "M4A",
  "AAC",
  "FLAC",
  "OGG",
  "AIFF",
  "MP4",
  "MOV",
] as const;

const pad2 = (value: number): string => String(value).padStart(2, "0");

/**
 * Media-player style clock: "M:SS" below one hour, "H:MM:SS" from one hour.
 * Rounds down to whole seconds; negative or non-finite input reads as 0.
 */
export const formatClock = (ms: number): string => {
  const totalSeconds =
    Number.isFinite(ms) && ms > 0 ? Math.floor(ms / 1000) : 0;
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  return hours > 0
    ? `${hours}:${pad2(minutes)}:${pad2(seconds)}`
    : `${minutes}:${pad2(seconds)}`;
};

/** Length of a recording, shown the same way as segment times. */
export const formatDuration = formatClock;

/** The last path component, accepting both "/" and "\" separators. */
export const fileNameFromPath = (path: string): string => {
  const trimmed = path.replace(/[\\/]+$/, "");
  const separator = Math.max(
    trimmed.lastIndexOf("/"),
    trimmed.lastIndexOf("\\"),
  );
  return trimmed.slice(separator + 1);
};

/**
 * Lower-case extension without the dot, or "" when there is none. Like
 * Rust's `Path::extension`, a leading dot (".wav") is not an extension.
 */
export const extensionOf = (path: string): string => {
  const name = fileNameFromPath(path);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
};

/**
 * Whether the backend can open this path, by extension. Formats that need
 * ffmpeg count even when ffmpeg is missing, so the job reports `needs_ffmpeg`
 * instead of the file being dropped silently.
 */
export const isSupportedPath = (
  path: string,
  formats: Pick<SupportedAudioFormats, "native" | "ffmpeg">,
): boolean => {
  const extension = extensionOf(path);
  return (
    extension !== "" &&
    (formats.native.includes(extension) || formats.ffmpeg.includes(extension))
  );
};

/**
 * Extensions for the file picker filter: what can be opened right now, in
 * both cases, because GTK file-chooser patterns are case-sensitive.
 */
export const pickerExtensions = (formats: SupportedAudioFormats): string[] => {
  const openable = formats.ffmpeg_available
    ? [...formats.native, ...formats.ffmpeg]
    : formats.native;
  const lower = openable.map((extension) => extension.toLowerCase());
  return Array.from(
    new Set([...lower, ...lower.map((extension) => extension.toUpperCase())]),
  );
};

// Same rule as `default_export_file_name` in file_transcription/export.rs:
// Windows-reserved characters and control characters (Rust `char::is_control`).
const UNSAFE_FILE_NAME_CHARS = /[<>:"/\\|?*\u0000-\u001f\u007f-\u009f]/g;

/** "interview.final.m4a" + "srt" -> "interview.final.srt". */
export const defaultExportName = (
  sourceName: string,
  format: TranscriptFormat,
): string => {
  const dot = sourceName.lastIndexOf(".");
  const stem = (dot > 0 ? sourceName.slice(0, dot) : sourceName).replace(
    UNSAFE_FILE_NAME_CHARS,
    "_",
  );
  return `${stem || "transcript"}.${format}`;
};

const RUNNING_STATUSES: readonly FileJobStatus[] = [
  "preparing",
  "waiting",
  "transcribing",
];
const FINISHED_STATUSES: readonly FileJobStatus[] = [
  "completed",
  "cancelled",
  "failed",
];

/** The job is being worked on (not queued, not finished). */
export const isRunningStatus = (status: FileJobStatus): boolean =>
  RUNNING_STATUSES.includes(status);

/** The job will not change any more; it can be dismissed. */
export const isFinishedStatus = (status: FileJobStatus): boolean =>
  FINISHED_STATUSES.includes(status);

/** Keys under `settings.transcribeFiles.status`. */
export type JobStatusKey = FileJobStatus | "partial";

/** A completed job whose transcript has gaps reads "partial". */
export const statusKey = (
  job: Pick<FileJobSnapshot, "status" | "transcript_status">,
): JobStatusKey =>
  job.status === "completed" && job.transcript_status === "partial"
    ? "partial"
    : job.status;

const ERROR_KEYS = {
  not_found: "notFound",
  unsupported_format: "unsupportedFormat",
  needs_ffmpeg: "needsFfmpeg",
  no_model: "noModel",
  model_changed: "modelChanged",
  decode_failed: "decodeFailed",
  engine_failed: "engineFailed",
  internal: "internal",
} as const satisfies Record<FileJobErrorCode, string>;

/** Keys under `settings.transcribeFiles.errors` for each job error code. */
export type JobErrorKey = (typeof ERROR_KEYS)[FileJobErrorCode];

export const errorKey = (code: FileJobErrorCode): JobErrorKey =>
  ERROR_KEYS[code];

/**
 * Segments of a running job: what `get_file_transcript` returned, followed by
 * the live segments from later chunk events. Both lists are in time order, so
 * anything starting at or before the last fetched segment is a duplicate.
 */
export const mergeLiveSegments = (
  fetched: TranscriptSegment[],
  live: TranscriptSegment[],
): TranscriptSegment[] => {
  if (live.length === 0) return fetched;
  if (fetched.length === 0) return live;
  const lastStart = fetched[fetched.length - 1].start_ms;
  const newer = live.filter((segment) => segment.start_ms > lastStart);
  return newer.length === 0 ? fetched : [...fetched, ...newer];
};

/**
 * The first `maxChars` UTF-16 units of `text`, never ending in half of a
 * surrogate pair. Keeps the DOM of clamped History previews small.
 */
export const previewText = (text: string, maxChars: number): string => {
  if (text.length <= maxChars) return text;
  const end =
    maxChars > 0 && /[\uD800-\uDBFF]/.test(text.charAt(maxChars - 1))
      ? maxChars - 1
      : maxChars;
  return text.slice(0, end);
};

/**
 * Wraps a value in Unicode directional isolates (LRI ... PDI) so times and
 * file names keep their left-to-right order inside right-to-left sentences.
 */
export const isolateLtr = (value: string): string => `\u2066${value}\u2069`;
