import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import i18next from "i18next";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { Trans } from "react-i18next";
import type {
  FileJobErrorCode,
  FileJobSnapshot,
  LemonadeErrorKind,
  ModelInfo,
  SpeakerJobState,
  SupportedAudioFormats,
  TranscriptSpeakers,
} from "@/bindings";
import {
  activeLabelJobForHistory,
  awaitingSpeakers,
  DEFAULT_TRANSCRIPT_AUTOSAVE,
  defaultExportName,
  downloadPercent,
  ESTIMATE_MIN_ACTIVE_MS,
  ESTIMATE_MIN_AUDIO_MS,
  errorKey,
  estimateRemainingMs,
  extensionOf,
  fileNameFromPath,
  formatClock,
  formatDuration,
  formatPercent,
  isFinishedStatus,
  isLabelJob,
  isLabelTarget,
  isLoopbackUrl,
  isolateLtr,
  isRunningStatus,
  isSupportedPath,
  isValidServerUrl,
  jobElapsedMs,
  jobProgressPercent,
  jobSpeakerView,
  labelJobFor,
  lemonadeErrorKey,
  menuKeyAction,
  mergeLiveSegments,
  notReadyKey,
  pickerExtensions,
  placeMenu,
  previewText,
  progressPercent,
  type RateEstimate,
  recordingsModelId,
  recordingsModelOptions,
  refocusTarget,
  runIfConfirmed,
  savedJobFor,
  serverUrlHost,
  speakerName,
  speakerSkipKey,
  speakerStyle,
  statusKey,
  timeLeftText,
  transcribeBadge,
  updateRateEstimate,
} from "./transcript";

// formatClock: "M:SS" under an hour, "H:MM:SS" from an hour, floored.
assert.equal(formatClock(0), "0:00");
assert.equal(formatClock(999), "0:00");
assert.equal(formatClock(1000), "0:01");
assert.equal(formatClock(59_999), "0:59");
assert.equal(formatClock(60_000), "1:00");
assert.equal(formatClock(754_000), "12:34");
assert.equal(formatClock(3_599_999), "59:59");
assert.equal(formatClock(3_600_000), "1:00:00");
assert.equal(formatClock(3_725_000), "1:02:05");
assert.equal(formatClock(360_000_000), "100:00:00");
assert.equal(formatClock(-5), "0:00");
assert.equal(formatClock(Number.NaN), "0:00");
assert.equal(formatDuration(3_725_000), "1:02:05");

// fileNameFromPath / extensionOf: both separators, case folding, no dot.
assert.equal(fileNameFromPath("/home/me/Talks/interview.m4a"), "interview.m4a");
assert.equal(fileNameFromPath("C:\\Users\\me\\Talk.MP3"), "Talk.MP3");
assert.equal(fileNameFromPath("C:\\Users\\me/mixed\\memo.wav"), "memo.wav");
assert.equal(fileNameFromPath("/music/album/"), "album");
assert.equal(fileNameFromPath("memo.wav"), "memo.wav");
assert.equal(extensionOf("/home/me/interview.m4a"), "m4a");
assert.equal(extensionOf("C:\\Users\\me\\Talk.MP3"), "mp3");
assert.equal(extensionOf("C:\\Users\\me.d\\Recording"), "");
assert.equal(extensionOf("/home/me.d/recording"), "");
assert.equal(extensionOf("/home/me/archive.tar.FLAC"), "flac");
assert.equal(extensionOf("/home/me/.wav"), "");
assert.equal(extensionOf("/home/me/trailing."), "");
assert.equal(extensionOf(""), "");

// isSupportedPath: native and ffmpeg lists count, whatever ffmpeg's state.
const formats: SupportedAudioFormats = {
  native: ["wav", "mp3", "m4a"],
  ffmpeg: ["opus", "webm"],
  ffmpeg_available: false,
  ffmpeg_path: null,
};
assert.equal(isSupportedPath("/a/b.wav", formats), true);
assert.equal(isSupportedPath("C:\\a\\B.MP3", formats), true);
assert.equal(isSupportedPath("/a/voice.opus", formats), true);
assert.equal(isSupportedPath("/a/notes.txt", formats), false);
assert.equal(isSupportedPath("/a/folder", formats), false);
assert.equal(isSupportedPath("/a/.wav", formats), false);

// pickerExtensions: ffmpeg formats only when ffmpeg is available; both cases.
assert.deepEqual(pickerExtensions(formats), [
  "wav",
  "mp3",
  "m4a",
  "WAV",
  "MP3",
  "M4A",
]);
assert.deepEqual(pickerExtensions({ ...formats, ffmpeg_available: true }), [
  "wav",
  "mp3",
  "m4a",
  "opus",
  "webm",
  "WAV",
  "MP3",
  "M4A",
  "OPUS",
  "WEBM",
]);

// defaultExportName: replaces the last extension and sanitises the stem.
assert.equal(
  defaultExportName("interview.final.m4a", "srt"),
  "interview.final.srt",
);
assert.equal(
  defaultExportName("interview.final.m4a", "docx"),
  "interview.final.docx",
);
assert.equal(defaultExportName("Meeting.MOV", "txt"), "Meeting.txt");
assert.equal(defaultExportName("no-extension", "md"), "no-extension.md");
assert.equal(
  defaultExportName('a<b>c:d"e|f?g*h.wav', "vtt"),
  "a_b_c_d_e_f_g_h.vtt",
);
assert.equal(defaultExportName("tab\there\u0085.mp3", "txt"), "tab_here_.txt");
assert.equal(defaultExportName("", "txt"), "transcript.txt");
assert.equal(defaultExportName(".wav", "txt"), ".wav.txt");

// statusKey: completed with gaps reads "partial"; everything else is its status.
assert.equal(
  statusKey({ status: "completed", transcript_status: "partial" }),
  "partial",
);
assert.equal(
  statusKey({ status: "completed", transcript_status: "completed" }),
  "completed",
);
assert.equal(
  statusKey({ status: "cancelled", transcript_status: "partial" }),
  "cancelled",
);
assert.equal(
  statusKey({ status: "waiting", transcript_status: null }),
  "waiting",
);

assert.equal(isRunningStatus("queued"), false);
assert.equal(isRunningStatus("preparing"), true);
assert.equal(isRunningStatus("waiting"), true);
assert.equal(isRunningStatus("transcribing"), true);
assert.equal(isRunningStatus("completed"), false);
assert.equal(isFinishedStatus("transcribing"), false);
assert.equal(isFinishedStatus("completed"), true);
assert.equal(isFinishedStatus("cancelled"), true);
assert.equal(isFinishedStatus("failed"), true);

// errorKey: every backend code maps to its camelCase i18n key.
const expectedErrorKeys: Record<FileJobErrorCode, string> = {
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
  source_changed: "sourceChanged",
  speaker_labels_failed: "speakerLabelsFailed",
};
for (const [code, key] of Object.entries(expectedErrorKeys)) {
  assert.equal(errorKey(code as FileJobErrorCode), key);
}

// mergeLiveSegments: appends only live segments newer than the fetched ones.
const segment = (start_ms: number, text: string) => ({
  start_ms,
  end_ms: start_ms + 1000,
  text,
});
const fetched = [segment(0, "a"), segment(5000, "b")];
assert.deepEqual(mergeLiveSegments(fetched, []), fetched);
assert.deepEqual(mergeLiveSegments([], fetched), fetched);
assert.deepEqual(
  mergeLiveSegments(fetched, [segment(5000, "b"), segment(9000, "c")]),
  [...fetched, segment(9000, "c")],
);
assert.deepEqual(
  mergeLiveSegments(fetched, [segment(0, "a"), segment(5000, "b")]),
  fetched,
);

// previewText: cuts long text without splitting a surrogate pair.
assert.equal(previewText("short", 10), "short");
assert.equal(previewText("abcdef", 3), "abc");
assert.equal(previewText("ab\u{1F600}cd", 3), "ab");
assert.equal(previewText("ab\u{1F600}cd", 4), "ab\u{1F600}");

assert.equal(isolateLtr("1:02"), "\u20661:02\u2069");

// placeMenu: fixed to the window, so a short dialog cannot clip the menu. It
// opens on the preferred side when it fits and flips when it does not.
const window680 = { width: 680, height: 570 };
const exportMenu = { width: 192, height: 142 };
const footerButton = { top: 400, bottom: 430, left: 500, right: 590 };
assert.deepEqual(placeMenu(footerButton, exportMenu, window680, "top"), {
  top: 254,
  left: 398,
  maxHeight: 388,
});
const rowButton = { top: 200, bottom: 230, left: 500, right: 590 };
assert.deepEqual(placeMenu(rowButton, exportMenu, window680, "bottom"), {
  top: 234,
  left: 398,
  maxHeight: 328,
});
const nearTop = { top: 60, bottom: 90, left: 500, right: 590 };
assert.deepEqual(placeMenu(nearTop, exportMenu, window680, "top"), {
  top: 94,
  left: 398,
  maxHeight: 468,
});
const nearBottom = { top: 500, bottom: 530, left: 500, right: 590 };
assert.deepEqual(placeMenu(nearBottom, exportMenu, window680, "bottom"), {
  top: 354,
  left: 398,
  maxHeight: 488,
});
// Too little room on both sides: the roomier side, with a capped height.
assert.deepEqual(
  placeMenu(nearTop, exportMenu, { width: 680, height: 180 }, "top"),
  { top: 94, left: 398, maxHeight: 78 },
);
// End-aligned with the trigger (left edge in RTL), kept inside the window.
const nearLeft = { top: 400, bottom: 430, left: 20, right: 60 };
assert.equal(placeMenu(nearLeft, exportMenu, window680, "top").left, 8);
assert.equal(placeMenu(nearLeft, exportMenu, window680, "top", true).left, 20);
const nearRight = { top: 400, bottom: 430, left: 640, right: 672 };
assert.equal(
  placeMenu(nearRight, exportMenu, window680, "top", true).left,
  480,
);
for (let top = 0; top <= 540; top += 15) {
  for (const preferred of ["top", "bottom"] as const) {
    const trigger = { top, bottom: top + 30, left: 500, right: 590 };
    const { top: menuTop, maxHeight } = placeMenu(
      trigger,
      exportMenu,
      window680,
      preferred,
    );
    const menuBottom = menuTop + Math.min(exportMenu.height, maxHeight);
    assert.ok(menuTop >= 8 && menuBottom <= 562, `${preferred} at ${top}`);
    assert.ok(menuBottom <= trigger.top || menuTop >= trigger.bottom);
  }
}

// menuKeyAction: arrows wrap, and Tab closes the menu like Escape (focus goes
// back to the trigger) instead of leaving a dialog around it.
assert.deepEqual(menuKeyAction("Tab", 1, 4), { kind: "close" });
assert.deepEqual(menuKeyAction("Escape", 0, 4), { kind: "close" });
assert.deepEqual(menuKeyAction("Tab", -1, 0), { kind: "close" });
assert.deepEqual(menuKeyAction("ArrowDown", 0, 4), { kind: "focus", index: 1 });
assert.deepEqual(menuKeyAction("ArrowDown", 3, 4), { kind: "focus", index: 0 });
assert.deepEqual(menuKeyAction("ArrowDown", -1, 4), {
  kind: "focus",
  index: 0,
});
assert.deepEqual(menuKeyAction("ArrowUp", 2, 4), { kind: "focus", index: 1 });
assert.deepEqual(menuKeyAction("ArrowUp", 0, 4), { kind: "focus", index: 3 });
assert.deepEqual(menuKeyAction("ArrowUp", -1, 4), { kind: "focus", index: 3 });
assert.deepEqual(menuKeyAction("Home", 2, 4), { kind: "focus", index: 0 });
assert.deepEqual(menuKeyAction("End", 0, 4), { kind: "focus", index: 3 });
assert.equal(menuKeyAction("ArrowDown", -1, 0), null);
assert.equal(menuKeyAction("Enter", 0, 4), null);
assert.equal(menuKeyAction("a", 0, 4), null);

// Lemonade Server addresses: http(s) with a host, the scheme written out.
assert.equal(serverUrlHost("http://localhost:13305"), "localhost");
assert.equal(
  serverUrlHost("  HTTPS://Lemonade.Example:8000/api/v1/ "),
  "lemonade.example",
);
assert.equal(serverUrlHost("http://[::1]:13305/v1"), "[::1]");
assert.equal(serverUrlHost("localhost:13305"), null);
assert.equal(serverUrlHost("ftp://localhost"), null);
assert.equal(serverUrlHost("http://"), null);
assert.equal(serverUrlHost("http:localhost"), null);
assert.equal(serverUrlHost(""), null);
assert.equal(isValidServerUrl("http://127.0.0.1:8000/api/v1"), true);
assert.equal(isValidServerUrl("http//localhost"), false);
// Everything `normalize_base_url` (lemonade.rs) refuses is refused here, so
// a typo shows the inline hint instead of a failed save: a query or fragment
// (even an empty one), and an API suffix that leaves no host.
for (const address of [
  "http://localhost:13305/?a=1",
  "http://localhost:13305?",
  "http://localhost:13305#",
  "http://localhost:13305/#/",
  "http://localhost:13305/#x",
  "http://localhost:13305/api/v1?x=1",
  "http://host/?q=1",
  "http:///api/v1",
  "http:///v1",
  "https://v1",
  "http://api/v1",
]) {
  assert.equal(isValidServerUrl(address), false, address);
}
for (const address of [
  "http://localhost:13305/",
  "http://localhost:8000/api/v1/",
  "https://box.lan/lemonade/v1",
  "HTTP://127.0.0.1:13305/API/V1",
  "http://h/v1/v1",
]) {
  assert.equal(isValidServerUrl(address), true, address);
}
assert.equal(serverUrlHost("https://Box.lan/lemonade/v1"), "box.lan");

// isLoopbackUrl: localhost, 127.0.0.0/8 and ::1 only; invalid reads false.
for (const address of [
  "http://localhost:13305",
  "http://LOCALHOST:13305/",
  "http://localhost.:13305",
  "https://127.0.0.1",
  "http://127.12.0.3:8000/api/v1",
  "http://127.1:13305",
  "http://[::1]:13305",
  "http://[0:0:0:0:0:0:0:1]:13305",
]) {
  assert.equal(isLoopbackUrl(address), true, address);
}
for (const address of [
  "http://192.168.1.20:13305",
  "http://10.0.0.5",
  "http://lemonade.local:13305",
  "http://localhost.example.com",
  "http://128.0.0.1",
  "http://[::2]",
  "localhost:13305",
  "",
]) {
  assert.equal(isLoopbackUrl(address), false, address);
}

// lemonadeErrorKey: every check error maps to its camelCase i18n key.
const expectedLemonadeKeys: Record<LemonadeErrorKind, string> = {
  invalid_url: "invalidUrl",
  unreachable: "unreachable",
  timeout: "timeout",
  unauthorized: "unauthorized",
  model_not_found: "modelNotFound",
  http: "http",
  bad_response: "badResponse",
  cancelled: "cancelled",
};
for (const [kind, key] of Object.entries(expectedLemonadeKeys)) {
  assert.equal(lemonadeErrorKey(kind as LemonadeErrorKind), key);
}
assert.equal(speakerSkipKey("model_missing"), "modelMissing");
assert.equal(speakerSkipKey("no_speech"), "noSpeech");

// jobSpeakerView: nothing unless requested; "running" only while the job runs;
// cancelled and failed jobs say nothing about labels.
const speakerJob = (
  status: FileJobSnapshot["status"],
  speakers: SpeakerJobState | null,
) => ({ status, speakers });
assert.equal(jobSpeakerView(speakerJob("transcribing", null)), null);
assert.equal(jobSpeakerView(speakerJob("queued", { state: "pending" })), null);
assert.deepEqual(
  jobSpeakerView(speakerJob("preparing", { state: "pending" })),
  { kind: "running" },
);
assert.deepEqual(jobSpeakerView(speakerJob("waiting", { state: "running" })), {
  kind: "running",
});
assert.deepEqual(
  jobSpeakerView(
    speakerJob("transcribing", { state: "skipped", reason: "model_missing" }),
  ),
  { kind: "skipped", reason: "model_missing" },
);
assert.deepEqual(
  jobSpeakerView(speakerJob("completed", { state: "done", speakers: 2 })),
  { kind: "done", speakers: 2 },
);
assert.deepEqual(
  jobSpeakerView(
    speakerJob("completed", { state: "failed", message: "worker crashed" }),
  ),
  { kind: "failed", message: "worker crashed" },
);
assert.equal(
  jobSpeakerView(speakerJob("completed", { state: "running" })),
  null,
);
assert.equal(
  jobSpeakerView(
    speakerJob("cancelled", { state: "failed", message: "cancelled" }),
  ),
  null,
);
assert.equal(jobSpeakerView(speakerJob("failed", { state: "running" })), null);
// A cancelled or failed job whose partial transcript was labelled later.
assert.deepEqual(
  jobSpeakerView(speakerJob("cancelled", { state: "done", speakers: 3 })),
  { kind: "done", speakers: 3 },
);
assert.deepEqual(
  jobSpeakerView(speakerJob("failed", { state: "done", speakers: 2 })),
  { kind: "done", speakers: 2 },
);
assert.equal(
  jobSpeakerView(speakerJob("queued", { state: "done", speakers: 2 })),
  null,
);

// Speaker-label jobs: found by the transcript they label.
const snapshot = (
  job_id: number,
  status: FileJobSnapshot["status"],
  labels_for: FileJobSnapshot["labels_for"],
): FileJobSnapshot => ({
  job_id,
  path: "/audio/talk.m4a",
  file_name: "talk.m4a",
  status,
  duration_ms: 60_000,
  processed_ms: 0,
  chunks_done: 0,
  decoder: null,
  model_id: null,
  engine: "handy",
  speakers: labels_for === null ? null : { state: "pending" },
  error: null,
  has_transcript: false,
  transcript_status: null,
  history_id: null,
  enqueued_at: job_id,
  labels_for,
  started_at: null,
  finished_at: null,
  speakers_processed_ms: null,
  saved_file: null,
  save_error: null,
});
const jobs: Record<number, FileJobSnapshot> = Object.fromEntries(
  [
    snapshot(1, "completed", null),
    snapshot(2, "failed", { kind: "job", job_id: 1 }),
    snapshot(3, "queued", { kind: "job", job_id: 1 }),
    snapshot(4, "completed", { kind: "history", history_id: 9 }),
    snapshot(5, "transcribing", { kind: "history", history_id: 7 }),
    snapshot(6, "cancelled", { kind: "job", job_id: 8 }),
  ].map((job) => [job.job_id, job]),
);
assert.equal(isLabelJob(jobs[1]), false);
assert.equal(isLabelJob(jobs[2]), true);
// The newest wins; transcription jobs never match.
assert.equal(labelJobFor(jobs, { kind: "job", job_id: 1 }, null)?.job_id, 3);
assert.equal(
  labelJobFor(jobs, { kind: "history", history_id: 9 }, 9)?.job_id,
  4,
);
// A job saved to History is labelled as its entry.
assert.equal(labelJobFor(jobs, { kind: "job", job_id: 12 }, 9)?.job_id, 4);
assert.equal(labelJobFor(jobs, { kind: "job", job_id: 12 }, null), null);
assert.equal(labelJobFor(jobs, { kind: "job", job_id: 4 }, undefined), null);
assert.equal(labelJobFor({}, { kind: "history", history_id: 9 }, 9), null);
// History rows: only an unfinished job for that entry.
assert.equal(activeLabelJobForHistory(jobs, 7)?.job_id, 5);
assert.equal(activeLabelJobForHistory(jobs, 9), null);
assert.equal(activeLabelJobForHistory(jobs, 1), null);
// Jobs an unfinished label job targets stay listed.
assert.equal(isLabelTarget(jobs, 1), true);
assert.equal(isLabelTarget(jobs, 8), false);
assert.equal(isLabelTarget(jobs, 2), false);

// awaitingSpeakers: transcribing, all audio processed, labels still coming.
const progressJob = {
  status: "transcribing" as const,
  speakers: { state: "running" } as SpeakerJobState,
  duration_ms: 60_000,
  processed_ms: 60_000,
};
assert.equal(awaitingSpeakers(progressJob), true);
assert.equal(
  awaitingSpeakers({ ...progressJob, speakers: { state: "pending" } }),
  true,
);
assert.equal(awaitingSpeakers({ ...progressJob, processed_ms: 59_000 }), false);
assert.equal(awaitingSpeakers({ ...progressJob, duration_ms: null }), false);
assert.equal(awaitingSpeakers({ ...progressJob, duration_ms: 0 }), false);
assert.equal(awaitingSpeakers({ ...progressJob, speakers: null }), false);
assert.equal(
  awaitingSpeakers({
    ...progressJob,
    speakers: { state: "done", speakers: 2 },
  }),
  false,
);
assert.equal(awaitingSpeakers({ ...progressJob, status: "completed" }), false);
// Speaker progress means the audio is done, even if the probed length was
// a little longer than the decoded audio.
assert.equal(
  awaitingSpeakers({
    ...progressJob,
    processed_ms: 59_500,
    speakers_processed_ms: 30_000,
  }),
  true,
);
assert.equal(
  awaitingSpeakers({
    ...progressJob,
    processed_ms: 59_500,
    speakers_processed_ms: null,
  }),
  false,
);

// progressPercent: whole percent, rounded down, null without a length.
assert.equal(progressPercent(0, 60_000), 0);
assert.equal(progressPercent(25_200, 60_000), 42);
assert.equal(progressPercent(59_999, 60_000), 99);
assert.equal(progressPercent(60_000, 60_000), 100);
assert.equal(progressPercent(90_000, 60_000), 100);
assert.equal(progressPercent(-5, 60_000), 0);
assert.equal(progressPercent(Number.NaN, 60_000), 0);
assert.equal(progressPercent(1_000, null), null);
assert.equal(progressPercent(1_000, 0), null);

// formatPercent: the locale's percent style.
assert.equal(formatPercent(42, "en"), "42%");
assert.equal(formatPercent(0, "en"), "0%");
assert.equal(formatPercent(100, "en"), "100%");
assert.equal(formatPercent(42, "fr").replace(/\s/g, " "), "42 %");
assert.equal(formatPercent(42, "not a locale!"), "42%");

// jobProgressPercent and the sidebar badge.
const runningJob = (
  overrides: Partial<FileJobSnapshot> = {},
): FileJobSnapshot => ({
  ...snapshot(20, "transcribing", null),
  processed_ms: 30_000,
  ...overrides,
});
assert.equal(jobProgressPercent(runningJob()), 50);
assert.equal(jobProgressPercent(runningJob({ status: "waiting" })), 50);
assert.equal(jobProgressPercent(runningJob({ status: "preparing" })), null);
assert.equal(jobProgressPercent(runningJob({ duration_ms: null })), null);
// The speaker step shows the diarizer's progress once it reports any.
const speakerStep = runningJob({
  processed_ms: 60_000,
  speakers: { state: "running" },
});
assert.equal(jobProgressPercent(speakerStep), null);
assert.equal(
  jobProgressPercent({ ...speakerStep, speakers_processed_ms: 45_000 }),
  75,
);
// A speaker-label job: once its first window is done.
const labelRunning = runningJob({
  labels_for: { kind: "history", history_id: 3 },
  speakers: { state: "running" },
  processed_ms: 0,
});
assert.equal(jobProgressPercent(labelRunning), null);
assert.equal(jobProgressPercent({ ...labelRunning, processed_ms: 15_000 }), 25);
assert.equal(transcribeBadge({}), null);
assert.equal(
  transcribeBadge({ 1: snapshot(1, "completed", null), 20: runningJob() }),
  50,
);
assert.equal(
  transcribeBadge({ 20: runningJob({ status: "preparing" }) }),
  "busy",
);
assert.equal(
  transcribeBadge({
    1: snapshot(1, "queued", null),
    2: snapshot(2, "failed", null),
  }),
  null,
);

// updateRateEstimate: no speed until 2 minutes of audio or 30 seconds of
// transcribing, then the average so far, then smoothed steps.
const t0 = 1_000_000;
let estimate: RateEstimate = updateRateEstimate(null, 0, true, t0);
assert.equal(estimate.rate, null);
// 28 s of audio in 7 s: not enough yet.
estimate = updateRateEstimate(estimate, 28_000, true, t0 + 7_000);
assert.equal(estimate.rate, null);
assert.equal(estimateRemainingMs(estimate, 600_000, t0 + 7_000), null);
// 2 minutes of audio (in 28 s): the average, 4x real time.
estimate = updateRateEstimate(estimate, 56_000, true, t0 + 14_000);
estimate = updateRateEstimate(estimate, 84_000, true, t0 + 21_000);
estimate = updateRateEstimate(estimate, 120_000, true, t0 + 30_000);
assert.ok(estimate.audioMs >= ESTIMATE_MIN_AUDIO_MS);
assert.equal(estimate.rate, 4);
// 480 s of audio left at 4x: 120 s, counting down until the next snapshot.
assert.equal(estimateRemainingMs(estimate, 600_000, t0 + 30_000), 120_000);
assert.equal(estimateRemainingMs(estimate, 600_000, t0 + 40_000), 110_000);
assert.equal(estimateRemainingMs(estimate, 600_000, t0 + 500_000), 0);
// A slower step (28 s in 14 s: 2x) moves the speed only part of the way.
estimate = updateRateEstimate(estimate, 148_000, true, t0 + 44_000);
assert.ok(estimate.rate !== null && estimate.rate < 4 && estimate.rate > 3);
// Paused for dictation: the pause does not count as transcribing time.
const paused = updateRateEstimate(estimate, 148_000, false, t0 + 50_000);
const resumed = updateRateEstimate(paused, 148_000, true, t0 + 350_000);
assert.equal(resumed.activeMs, paused.activeMs);
const afterPause = updateRateEstimate(resumed, 176_000, true, t0 + 357_000);
// 28 s in the 13 s transcribed since the last step (6 + 7), not 313 s.
assert.equal(afterPause.sinceAdvanceMs, 0);
assert.ok(afterPause.rate !== null && afterPause.rate > 2.5);
// While paused, the estimate does not count down.
assert.equal(
  estimateRemainingMs(paused, 600_000, t0 + 50_000),
  estimateRemainingMs(paused, 600_000, t0 + 90_000),
);
// 30 s of transcribing is enough even for little audio (slow models).
let slow = updateRateEstimate(null, 0, true, t0);
slow = updateRateEstimate(slow, 10_000, true, t0 + ESTIMATE_MIN_ACTIVE_MS);
assert.equal(slow.rate, 10_000 / ESTIMATE_MIN_ACTIVE_MS);
// Time before the first snapshot, or spent preparing, never counts.
let preparing = updateRateEstimate(null, 0, false, t0);
preparing = updateRateEstimate(preparing, 0, true, t0 + 60_000);
assert.equal(preparing.activeMs, 0);
// A job that went back (a retry) starts again.
assert.equal(updateRateEstimate(estimate, 0, true, t0 + 60_000).rate, null);
assert.equal(estimateRemainingMs(null, 600_000, t0), null);
assert.equal(estimateRemainingMs(estimate, null, t0), null);

// timeLeftText: to the nearest minute.
assert.deepEqual(timeLeftText(0), { key: "lessThanMinute", values: {} });
assert.deepEqual(timeLeftText(59_999), { key: "lessThanMinute", values: {} });
assert.deepEqual(timeLeftText(60_000), {
  key: "minutes",
  values: { minutes: 1 },
});
assert.deepEqual(timeLeftText(3.6 * 60_000), {
  key: "minutes",
  values: { minutes: 4 },
});
assert.deepEqual(timeLeftText(59.4 * 60_000), {
  key: "minutes",
  values: { minutes: 59 },
});
assert.deepEqual(timeLeftText(59.6 * 60_000), {
  key: "hoursOnly",
  values: { hours: 1 },
});
assert.deepEqual(timeLeftText(85 * 60_000), {
  key: "hours",
  values: { hours: 1, minutes: 25 },
});
assert.deepEqual(timeLeftText(Number.NaN), {
  key: "lessThanMinute",
  values: {},
});

// jobElapsedMs: since the start, or until the finish.
assert.equal(jobElapsedMs(null, null, 5_000), null);
assert.equal(jobElapsedMs(1_000, null, 5_000), 4_000);
assert.equal(jobElapsedMs(1_000, 3_500, 9_000), 2_500);
assert.equal(jobElapsedMs(6_000, null, 5_000), 0);
assert.equal(formatClock(jobElapsedMs(0, null, 750_000) ?? 0), "12:30");

// savedJobFor: the job, or the newest job saved as the History entry.
const savedFile = (path: string) => ({ path, unwritable_folder: null });
const savedJobs: Record<number, FileJobSnapshot> = {
  1: { ...snapshot(1, "completed", null), saved_file: savedFile("/a.docx") },
  2: {
    ...snapshot(2, "completed", null),
    history_id: 9,
    saved_file: savedFile("/b.docx"),
  },
  3: {
    ...snapshot(3, "completed", null),
    history_id: 9,
    saved_file: savedFile("/b (2).docx"),
  },
  4: { ...snapshot(4, "completed", null), save_error: "disk full" },
  5: snapshot(5, "completed", null),
};
assert.equal(
  savedJobFor(savedJobs, { kind: "job", job_id: 1 }, null)?.job_id,
  1,
);
assert.equal(
  savedJobFor(savedJobs, { kind: "job", job_id: 4 }, null)?.job_id,
  4,
);
assert.equal(savedJobFor(savedJobs, { kind: "job", job_id: 5 }, null), null);
assert.equal(savedJobFor(savedJobs, { kind: "job", job_id: 99 }, null), null);
assert.equal(
  savedJobFor(savedJobs, { kind: "history", history_id: 9 }, 9)?.job_id,
  3,
);
assert.equal(
  savedJobFor(savedJobs, { kind: "history", history_id: 8 }, 8),
  null,
);

// The auto-save defaults match settings.rs.
assert.deepEqual(DEFAULT_TRANSCRIPT_AUTOSAVE, {
  enabled: true,
  location: "next_to_recording",
  folder: "",
  format: "docx",
});

// downloadPercent: whole percent, clamped, 0 while the total is unknown.
assert.equal(downloadPercent(0, 139_310_336), 0);
assert.equal(downloadPercent(69_655_168, 139_310_336), 50);
assert.equal(downloadPercent(139_310_335, 139_310_336), 99);
assert.equal(downloadPercent(139_310_336, 139_310_336), 100);
assert.equal(downloadPercent(200, 100), 100);
assert.equal(downloadPercent(50, 0), 0);

// The speaker model's license note: the model page link holds its text in
// every language. A void HTML tag name such as <link> leaves the button
// empty and the text outside it.
const localesDir = new URL("../../i18n/locales/", import.meta.url);
const languages = readdirSync(localesDir);
assert.ok(languages.length >= 26);
const localeI18n = i18next.createInstance();
await localeI18n.init({
  resources: Object.fromEntries(
    languages.map((lng) => [
      lng,
      {
        translation: JSON.parse(
          readFileSync(new URL(`${lng}/translation.json`, localesDir), "utf8"),
        ),
      },
    ]),
  ),
  interpolation: { escapeValue: false },
});
for (const lng of languages) {
  const html = renderToStaticMarkup(
    createElement(Trans, {
      i18n: localeI18n,
      t: localeI18n.getFixedT(lng),
      i18nKey: "settings.transcribeFiles.speakers.license",
      components: { page: createElement("button", { type: "button" }) },
    }),
  );
  const link = /<button type="button">([^<]*)<\/button>/.exec(html);
  assert.ok(link !== null && link[1].trim() !== "", `${lng}: ${html}`);
  assert.ok(html.includes("NVIDIA Open Model License"), `${lng}: ${html}`);
}

// runIfConfirmed: removing the speaker model waits for the user's yes.
{
  let removed = 0;
  const remove = async () => {
    removed += 1;
  };
  assert.equal(await runIfConfirmed(async () => false, remove), false);
  assert.equal(removed, 0);
  assert.equal(await runIfConfirmed(async () => true, remove), true);
  assert.equal(removed, 1);
}

// refocusTarget: focus lost with a replaced control goes to the row's new
// control, or to the fallback (the toggle) when the row is gone.
assert.equal(refocusTarget(true, true, "download", "toggle"), "download");
assert.equal(refocusTarget(true, true, null, "toggle"), "toggle");
assert.equal(refocusTarget(true, true, undefined, "toggle"), "toggle");
assert.equal(refocusTarget(true, true, undefined, undefined), null);
assert.equal(refocusTarget(true, false, null, "toggle"), null);
assert.equal(refocusTarget(false, true, "download", "toggle"), null);

// notReadyKey: each engine names what it is missing; the other engine's
// model doesn't matter.
assert.equal(notReadyKey("lemonade", "", true, false), "noLemonadeModel");
assert.equal(notReadyKey("lemonade", "", false, false), "noLemonadeModel");
assert.equal(notReadyKey("lemonade", "", true, true), "noLemonadeModel");
assert.equal(notReadyKey("lemonade", "Whisper-Base", false, true), null);
assert.equal(notReadyKey("handy", "", false, false), "noModel");
assert.equal(notReadyKey("handy", "", true, false), null);
assert.equal(notReadyKey("handy", "Whisper-Base", false, false), "noModel");
// A model chosen for recordings that isn't ready says so instead.
assert.equal(notReadyKey("handy", "", false, true), "noRecordingsModel");
assert.equal(notReadyKey("handy", "", true, true), null);

// recordingsModelId: the chosen model, else the dictation model.
assert.equal(recordingsModelId("", "small"), "small");
assert.equal(recordingsModelId("  ", "small"), "small");
assert.equal(recordingsModelId("cohere", "small"), "cohere");
assert.equal(recordingsModelId(" cohere ", ""), "cohere");

// recordingsModelOptions: "Same as dictation" first, then the downloaded
// transcribe-cpp models, the ones recommended for recordings first, each
// group by name; ONNX and missing models are not offered. Descriptions end in
// whether the model gives timestamps.
{
  const model = (
    id: string,
    name: string,
    engineType: ModelInfo["engine_type"],
    isDownloaded: boolean,
    extra: Partial<ModelInfo> = {},
  ): ModelInfo => ({
    id,
    name,
    description: `${name} description`,
    filename: `${id}.gguf`,
    source: "Local",
    size_mb: 100,
    is_downloaded: isDownloaded,
    is_downloading: false,
    partial_size: 0,
    is_directory: false,
    engine_type: engineType,
    accuracy_score: 0.5,
    speed_score: 0.5,
    supports_translation: false,
    is_recommended: false,
    supported_languages: ["en"],
    supports_language_selection: false,
    is_custom: false,
    supports_streaming: false,
    supports_language_detection: false,
    ...extra,
  });
  const models = [
    model("zeta", "Zeta GGUF", "TranscribeCpp", true),
    model("parakeet-tdt-0.6b-v3", "Parakeet V3", "Parakeet", true, {
      supports_timestamps: true,
      is_recommended_for_recordings: true,
    }),
    model("small", "Whisper Small", "TranscribeCpp", true, {
      supports_timestamps: true,
    }),
    model("alpha", "Alpha GGUF", "TranscribeCpp", true),
    model("medium", "Whisper Medium", "TranscribeCpp", false),
  ];
  const en = localeI18n.getFixedT("en");

  const options = recordingsModelOptions(
    models,
    "parakeet-tdt-0.6b-v3",
    "",
    en,
  );
  assert.deepEqual(
    options.map((option) => option.value),
    ["", "alpha", "small", "zeta"],
  );
  assert.equal(options[0].label, "Same as dictation (Parakeet V3)");
  assert.equal(options[0].disabled, undefined);
  // "Same as dictation" says whether the dictation model gives timestamps.
  assert.equal(options[0].description, "(timestamps)");
  // Catalog models use their translated name and description.
  assert.equal(options[2].label, "Whisper Small");
  assert.equal(
    options[2].description,
    "Fast and fairly accurate. (timestamps)",
  );
  // A model without the field (older backend) counts as untimed.
  assert.equal(
    options[1].description,
    "Alpha GGUF description (no timestamps)",
  );
  assert.ok(options.every((option) => option.disabled !== true));

  // No dictation model: nothing to say about timestamps.
  const noDictation = recordingsModelOptions(models, "", "", en)[0];
  assert.equal(noDictation.label, "Same as dictation");
  assert.equal(noDictation.description, undefined);
  assert.equal(
    recordingsModelOptions(models, "small", "", en)[0].description,
    "(timestamps)",
  );
  assert.equal(
    recordingsModelOptions(models, "zeta", "", en)[0].description,
    "(no timestamps)",
  );
  // The dictation model is offered like any other.
  assert.ok(
    recordingsModelOptions(models, "small", "small", en).some(
      (option) => option.value === "small" && option.disabled !== true,
    ),
  );

  // A chosen model that isn't usable (deleted, or edited to an ONNX model)
  // stays listed, disabled, by name or by id.
  const notDownloaded = recordingsModelOptions(models, "small", "medium", en);
  assert.deepEqual(notDownloaded[notDownloaded.length - 1], {
    value: "medium",
    label: "Whisper Medium",
    disabled: true,
  });
  const onnx = recordingsModelOptions(
    models,
    "small",
    "parakeet-tdt-0.6b-v3",
    en,
  );
  assert.equal(onnx[onnx.length - 1].label, "Parakeet V3");
  assert.equal(onnx[onnx.length - 1].disabled, true);
  const gone = recordingsModelOptions(models, "small", "gone-model", en);
  assert.equal(gone.length, 5);
  assert.deepEqual(gone[gone.length - 1], {
    value: "gone-model",
    label: "gone-model",
    disabled: true,
  });

  // Labels follow the UI language.
  const de = localeI18n.getFixedT("de");
  const german = recordingsModelOptions(models, "small", "", de)[0].label;
  assert.notEqual(german, "Same as dictation (Whisper Small)");
  assert.ok(german.includes("Whisper Small"), german);
  const germanSmall = recordingsModelOptions(models, "small", "", de).find(
    (option) => option.value === "small",
  );
  assert.ok(germanSmall?.description?.endsWith("(Zeitstempel)"));

  // Models recommended for recordings come first, each group by name.
  const picks = [
    model("whisper-tiny", "Whisper Tiny", "TranscribeCpp", true, {
      supports_timestamps: true,
    }),
    model("turbo-gguf", "Whisper Large v3 Turbo", "TranscribeCpp", true, {
      supports_timestamps: true,
      is_recommended_for_recordings: true,
    }),
    model("canary", "Canary 180M Flash", "TranscribeCpp", true),
    model("v3-gguf", "Parakeet TDT 0.6B v3", "TranscribeCpp", true, {
      supports_timestamps: true,
      is_recommended_for_recordings: true,
    }),
    // Recommended but not downloaded: not offered.
    model("large-gguf", "Whisper Large v3", "TranscribeCpp", false, {
      supports_timestamps: true,
      is_recommended_for_recordings: true,
    }),
    model("alpha", "Alpha GGUF", "TranscribeCpp", true),
  ];
  const ordered = recordingsModelOptions(picks, "canary", "", en);
  assert.deepEqual(
    ordered.map((option) => option.value),
    ["", "v3-gguf", "turbo-gguf", "alpha", "canary", "whisper-tiny"],
  );
  assert.deepEqual(
    ordered.map((option) => option.description),
    [
      "(no timestamps)",
      "Parakeet TDT 0.6B v3 description (timestamps)",
      "Whisper Large v3 Turbo description (timestamps)",
      "Alpha GGUF description (no timestamps)",
      "Canary 180M Flash description (no timestamps)",
      "Whisper Tiny description (timestamps)",
    ],
  );
  // A chosen model that went away still comes last, after every group.
  const withGone = recordingsModelOptions(picks, "canary", "large-gguf", en);
  assert.deepEqual(withGone[withGone.length - 1], {
    value: "large-gguf",
    label: "Whisper Large v3",
    disabled: true,
  });
}

// speakerName: the given name (trimmed), else the default for that id.
const transcriptSpeakers: TranscriptSpeakers = {
  speakers: [
    { id: 1, name: "Doug" },
    { id: 2, name: null },
    { id: 3, name: "   " },
  ],
  approximate: false,
  unassigned_segments: 0,
  model: "sortformer-4spk-v2.1-q8_0",
};
const defaultName = (id: number) => `Speaker ${id}`;
assert.equal(speakerName(transcriptSpeakers, 1, defaultName), "Doug");
assert.equal(speakerName(transcriptSpeakers, 2, defaultName), "Speaker 2");
assert.equal(speakerName(transcriptSpeakers, 3, defaultName), "Speaker 3");
assert.equal(speakerName(transcriptSpeakers, 7, defaultName), "Speaker 7");
assert.equal(speakerName(null, 1, defaultName), "Speaker 1");

// speakerStyle: one colour per speaker, repeating after four.
const styles = [1, 2, 3, 4].map(speakerStyle);
assert.equal(new Set(styles).size, 4);
assert.ok(styles.every((style) => style.startsWith("text-[")));
assert.equal(speakerStyle(5), speakerStyle(1));
assert.equal(speakerStyle(8), speakerStyle(4));
assert.equal(speakerStyle(0), speakerStyle(1));
assert.equal(speakerStyle(Number.NaN), speakerStyle(1));

console.log("transcript: all assertions passed");
