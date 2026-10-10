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
  SpeakerJobState,
  SupportedAudioFormats,
  TranscriptSpeakers,
} from "@/bindings";
import {
  awaitingSpeakers,
  defaultExportName,
  downloadPercent,
  errorKey,
  extensionOf,
  fileNameFromPath,
  formatClock,
  formatDuration,
  isFinishedStatus,
  isLoopbackUrl,
  isolateLtr,
  isRunningStatus,
  isSupportedPath,
  isValidServerUrl,
  jobSpeakerView,
  lemonadeErrorKey,
  menuKeyAction,
  mergeLiveSegments,
  notReadyKey,
  pickerExtensions,
  placeMenu,
  previewText,
  refocusTarget,
  runIfConfirmed,
  serverUrlHost,
  speakerName,
  speakerSkipKey,
  speakerStyle,
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
assert.equal(notReadyKey("lemonade", "", true), "noLemonadeModel");
assert.equal(notReadyKey("lemonade", "", false), "noLemonadeModel");
assert.equal(notReadyKey("lemonade", "Whisper-Base", false), null);
assert.equal(notReadyKey("handy", "", false), "noModel");
assert.equal(notReadyKey("handy", "", true), null);
assert.equal(notReadyKey("handy", "Whisper-Base", false), "noModel");

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
