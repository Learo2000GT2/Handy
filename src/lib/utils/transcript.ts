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
  LemonadeErrorKind,
  LemonadeSettings,
  SpeakerSkipReason,
  SupportedAudioFormats,
  TranscriptFormat,
  TranscriptSegment,
  TranscriptSpeakers,
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
  server_unreachable: "serverUnreachable",
  server_unauthorized: "serverUnauthorized",
  server_model_missing: "serverModelMissing",
  internal: "internal",
} as const satisfies Record<FileJobErrorCode, string>;

/** Keys under `settings.transcribeFiles.errors` for each job error code. */
export type JobErrorKey = (typeof ERROR_KEYS)[FileJobErrorCode];

export const errorKey = (code: FileJobErrorCode): JobErrorKey =>
  ERROR_KEYS[code];

/** Lemonade settings before the user changes them (as in `settings.rs`). */
export const DEFAULT_LEMONADE_SETTINGS: LemonadeSettings = {
  base_url: "http://localhost:13305",
  model: "",
  api_key: "",
};

/**
 * Lower-case host of a Lemonade Server address, or null when the address is
 * not an http(s) URL with a host. The scheme must be written out, as the
 * backend requires.
 */
export const serverUrlHost = (address: string): string | null => {
  const trimmed = address.trim();
  if (!/^https?:\/\//i.test(trimmed)) return null;
  try {
    const url = new URL(trimmed);
    return url.hostname === "" ? null : url.hostname.toLowerCase();
  } catch {
    return null;
  }
};

/** The address can be saved as a Lemonade Server address. */
export const isValidServerUrl = (address: string): boolean =>
  serverUrlHost(address) !== null;

/**
 * The address points at this computer: "localhost", 127.0.0.0/8 or ::1.
 * False for anything else, including invalid addresses. URL parsing has
 * already normalised forms such as "127.1" and "[0:0:0:0:0:0:0:1]".
 */
export const isLoopbackUrl = (address: string): boolean => {
  const host = serverUrlHost(address)?.replace(/\.$/, "");
  if (host === undefined) return false;
  return (
    host === "localhost" ||
    host === "[::1]" ||
    /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host)
  );
};

const LEMONADE_ERROR_KEYS = {
  invalid_url: "invalidUrl",
  unreachable: "unreachable",
  timeout: "timeout",
  unauthorized: "unauthorized",
  model_not_found: "modelNotFound",
  http: "http",
  bad_response: "badResponse",
  cancelled: "cancelled",
} as const satisfies Record<LemonadeErrorKind, string>;

/** Keys under `settings.transcribeFiles.lemonade.errors`. */
export const lemonadeErrorKey = (
  kind: LemonadeErrorKind,
): (typeof LEMONADE_ERROR_KEYS)[LemonadeErrorKind] => LEMONADE_ERROR_KEYS[kind];

const SPEAKER_SKIP_KEYS = {
  model_missing: "modelMissing",
  no_speech: "noSpeech",
} as const satisfies Record<SpeakerSkipReason, string>;

/** Keys under `settings.transcribeFiles.speakers.state.skipped`. */
export const speakerSkipKey = (
  reason: SpeakerSkipReason,
): (typeof SPEAKER_SKIP_KEYS)[SpeakerSkipReason] => SPEAKER_SKIP_KEYS[reason];

/** What a job row says about speaker labels. */
export type JobSpeakerView =
  | { kind: "running" }
  | { kind: "done"; speakers: number }
  | { kind: "failed"; message: string }
  | { kind: "skipped"; reason: SpeakerSkipReason };

/**
 * The speaker label line of a job row, or null for none: labels were not
 * requested, the job has not started, or it was cancelled or failed (its
 * transcript is not labelled either way).
 */
export const jobSpeakerView = (
  job: Pick<FileJobSnapshot, "status" | "speakers">,
): JobSpeakerView | null => {
  const state = job.speakers;
  const running = isRunningStatus(job.status);
  if (state === null || (!running && job.status !== "completed")) {
    return null;
  }
  switch (state.state) {
    case "pending":
    case "running":
      return running ? { kind: "running" } : null;
    case "done":
      return { kind: "done", speakers: state.speakers };
    case "failed":
      return { kind: "failed", message: state.message };
    case "skipped":
      return { kind: "skipped", reason: state.reason };
  }
};

/**
 * All the audio is transcribed and the job only waits for its speaker labels,
 * so its progress reads "Identifying speakers…" rather than "10:00 of 10:00".
 */
export const awaitingSpeakers = (
  job: Pick<
    FileJobSnapshot,
    "status" | "speakers" | "duration_ms" | "processed_ms"
  >,
): boolean =>
  job.status === "transcribing" &&
  job.duration_ms !== null &&
  job.duration_ms > 0 &&
  job.processed_ms >= job.duration_ms &&
  (job.speakers?.state === "pending" || job.speakers?.state === "running");

/** Whole percent of a download; 0 while the total is unknown. */
export const downloadPercent = (downloaded: number, total: number): number =>
  total > 0 && downloaded > 0
    ? Math.min(100, Math.floor((downloaded / total) * 100))
    : 0;

/**
 * Display name of speaker `id`: the name the user gave, or `defaultName(id)`
 * (the translated "Speaker 2").
 */
export const speakerName = (
  speakers: TranscriptSpeakers | null | undefined,
  id: number,
  defaultName: (id: number) => string,
): string => {
  const name = speakers?.speakers.find((speaker) => speaker.id === id)?.name;
  const trimmed = name?.trim() ?? "";
  return trimmed !== "" ? trimmed : defaultName(id);
};

// Text colours for speakers 1-4, repeating from the first after that. Each
// hue (Tailwind's sky, emerald, amber and violet 500) is mixed with the theme's
// text colour, so it darkens on the light theme and lightens on the dark one,
// including when the theme setting overrides the system (`dark:` follows only
// the system). The name is always shown too: colour is never the only cue.
const SPEAKER_STYLES = [
  "text-[color-mix(in_srgb,var(--color-text)_40%,#0ea5e9)]",
  "text-[color-mix(in_srgb,var(--color-text)_40%,#10b981)]",
  "text-[color-mix(in_srgb,var(--color-text)_40%,#f59e0b)]",
  "text-[color-mix(in_srgb,var(--color-text)_40%,#8b5cf6)]",
] as const;

/** Tailwind text colour classes for speaker `id` (1-based). */
export const speakerStyle = (id: number): string => {
  const index = Number.isInteger(id) && id > 0 ? (id - 1) % 4 : 0;
  return SPEAKER_STYLES[index];
};

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

/** Viewport box of a menu trigger, as from `getBoundingClientRect()`. */
export interface AnchorRect {
  top: number;
  bottom: number;
  left: number;
  right: number;
}

export interface MenuPosition {
  top: number;
  left: number;
  /** Caps the menu's height when neither side has room for all of it. */
  maxHeight: number;
}

// Space between the trigger and the menu, and between the menu and the edge
// of the window (px).
const MENU_GAP = 4;
const MENU_MARGIN = 8;

/**
 * Fixed (viewport) position of a menu aligned with the end edge of its
 * trigger. It opens on the `preferred` side when it fits there and otherwise
 * on the side with more room, so it never runs past the window edges.
 */
export const placeMenu = (
  trigger: AnchorRect,
  menu: { width: number; height: number },
  viewport: { width: number; height: number },
  preferred: "top" | "bottom",
  rtl = false,
): MenuPosition => {
  const room = {
    top: Math.max(0, trigger.top - MENU_GAP - MENU_MARGIN),
    bottom: Math.max(
      0,
      viewport.height - trigger.bottom - MENU_GAP - MENU_MARGIN,
    ),
  };
  const other = preferred === "top" ? "bottom" : "top";
  const side =
    menu.height > room[preferred] && room[other] > room[preferred]
      ? other
      : preferred;
  const height = Math.min(menu.height, room[side]);
  const top =
    side === "top"
      ? trigger.top - MENU_GAP - height
      : trigger.bottom + MENU_GAP;

  const start = rtl ? trigger.left : trigger.right - menu.width;
  const left = Math.max(
    MENU_MARGIN,
    Math.min(start, viewport.width - MENU_MARGIN - menu.width),
  );

  return { top, left, maxHeight: room[side] };
};

export type MenuKeyAction =
  | { kind: "focus"; index: number }
  | { kind: "close" };

/**
 * What a key press does in an open menu of `count` items whose item `current`
 * has focus (-1 if none). Null leaves the key alone. "close" shuts the menu
 * and returns focus to its trigger: for Tab too, so focus stays inside a
 * dialog around the menu instead of leaving it from the unfocusable items.
 */
export const menuKeyAction = (
  key: string,
  current: number,
  count: number,
): MenuKeyAction | null => {
  if (key === "Escape" || key === "Tab") return { kind: "close" };
  if (count === 0) return null;
  switch (key) {
    case "ArrowDown":
      return { kind: "focus", index: (current + 1) % count };
    case "ArrowUp":
      return { kind: "focus", index: current <= 0 ? count - 1 : current - 1 };
    case "Home":
      return { kind: "focus", index: 0 };
    case "End":
      return { kind: "focus", index: count - 1 };
    default:
      return null;
  }
};
