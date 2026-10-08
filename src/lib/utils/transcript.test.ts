import assert from "node:assert/strict";
import type { FileJobErrorCode, SupportedAudioFormats } from "@/bindings";
import {
  defaultExportName,
  errorKey,
  extensionOf,
  fileNameFromPath,
  formatClock,
  formatDuration,
  isFinishedStatus,
  isolateLtr,
  isRunningStatus,
  isSupportedPath,
  mergeLiveSegments,
  pickerExtensions,
  previewText,
  statusKey,
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
  internal: "internal",
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

console.log("transcript: all assertions passed");
