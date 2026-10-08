//! The headless CLI path (`--transcribe-file`, `--list-devices`,
//! `--list-models`), run on a worker thread when Handy starts in headless mode.

use crate::cli::CliArgs;
use crate::engine_supervisor::WorkToken;
use crate::file_transcription::decode::{self, DecodeError, DecodeOptions};
use crate::file_transcription::export;
use crate::file_transcription::pipeline::{
    self, PipelineError, PipelineEvent, PipelineOptions, PipelineOutput,
};
use crate::file_transcription::types::TranscriptFormat;
use crate::managers::model::ModelManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// Shortest time between two redraws of the progress line.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Headless one-shot transcription for the `--transcribe-file` / `--list-devices`
/// path. Runs the same file transcription pipeline as the app; no mic, no
/// download. Returns a process exit code (0 ok, 1 runtime failure, 2 bad
/// input/usage).
pub(crate) fn run_headless_transcription(app: &AppHandle, args: &CliArgs) -> i32 {
    // --list-devices: print registered compute devices (with indices) and exit.
    // Useful on multi-GPU machines to discover the index for --device-index.
    if args.list_devices {
        let tm = app.state::<Arc<TranscriptionManager>>();
        let devices = match crate::managers::transcription::describe_compute_devices(&tm) {
            Ok(devices) => devices,
            Err(e) => {
                eprintln!("error: {}", e);
                return 1;
            }
        };
        if devices.is_empty() {
            println!("No transcribe-cpp compute devices registered.");
        } else {
            println!("transcribe-cpp compute devices:");
            for d in &devices {
                println!("  {}", d);
            }
        }
        if args.transcribe_file.is_none() {
            return 0;
        }
    }

    // --list-models: print the model registry (catalog + on-disk + custom) with
    // their ids — the same ids `--model` accepts — then exit. `--json` emits the
    // full ModelInfo array for scripting.
    if args.list_models {
        let model_manager = app.state::<Arc<ModelManager>>();
        let models = model_manager.get_available_models();
        if args.json {
            match serde_json::to_string_pretty(&models) {
                Ok(s) => println!("{}", s),
                Err(e) => {
                    eprintln!("error: failed to serialize models: {}", e);
                    return 1;
                }
            }
        } else if models.is_empty() {
            println!("No models available.");
        } else {
            println!("Available models (✓ = installed):");
            let width = models.iter().map(|m| m.id.len()).max().unwrap_or(0);
            for m in &models {
                let mark = if m.is_downloaded { "✓" } else { " " };
                let rec = if m.is_recommended {
                    "  [recommended]"
                } else {
                    ""
                };
                println!(
                    "  {}  {:<width$}  {}{}",
                    mark,
                    m.id,
                    m.name,
                    rec,
                    width = width
                );
            }
        }
        if args.transcribe_file.is_none() {
            return 0;
        }
    }

    match args.transcribe_file.as_deref() {
        Some(path) => transcribe_file(app, args, path),
        None => 0,
    }
}

/// `--transcribe-file`: probe, load the model, run the pipeline `--repeat`
/// times, then print or write the result as the flags ask.
fn transcribe_file(app: &AppHandle, args: &CliArgs, path: &Path) -> i32 {
    // Usage and output checks first: nothing below is cheap.
    let plan = match output_plan(args) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    if let Destination::File { path: output, .. } = &plan.destination {
        if let Err(e) = check_output_path(output, path) {
            eprintln!("error: {}", e);
            return 2;
        }
    }

    // A missing, unsupported or ffmpeg-only file fails before the model loads.
    let decode_options = DecodeOptions {
        ffmpeg: args.ffmpeg.clone(),
    };
    let probe = match decode::probe(path, &decode_options) {
        Ok(probe) => probe,
        Err(e) => {
            eprintln!("error: {}", probe_error(path, &e));
            return 2;
        }
    };

    let tm = app.state::<Arc<TranscriptionManager>>();

    let model_id = args
        .model
        .clone()
        .unwrap_or_else(|| get_settings(app).selected_model);
    if model_id.is_empty() {
        eprintln!("error: no model selected (pass --model or pick one in the app)");
        return 2;
    }

    // --device-index hard-selects a compute device by its --list-devices registry
    // index (transcribe-cpp / whisper-family models only; not persisted). Omit it
    // to use the persisted accelerator setting.
    let device_index = args.device_index;
    let requested_device = match device_index {
        Some(idx) => format!("index {}", idx),
        None => "settings".to_string(),
    };

    // Cold load (timed).
    let load_start = Instant::now();
    if let Err(e) = tm.load_model_with_device(&model_id, device_index) {
        eprintln!("error: load_model('{}') failed: {}", model_id, e);
        return 1;
    }
    let load_ms = load_start.elapsed().as_millis() as u64;
    let bound_backend = tm.current_backend();

    // Hold the model for the whole command, so an "Immediately" unload timeout
    // doesn't unload (and the next run reload) it between --repeat runs. The
    // lease also reports the model's chunk limits.
    let lease = match tm.begin_file_job(&model_id, device_index, WorkToken::new()) {
        Ok(lease) => lease,
        Err(e) => {
            eprintln!("error: preparing model '{}' failed: {}", model_id, e);
            return 1;
        }
    };
    if let Some(warning) = chunk_warning(
        plan.max_chunk_ms,
        lease.limits().hard_max_ms,
        probe.duration_ms,
    ) {
        eprintln!("warning: {}", warning);
    }

    let options = PipelineOptions {
        model_id: model_id.clone(),
        device_index,
        max_chunk_ms: plan.max_chunk_ms,
        ffmpeg: args.ffmpeg.clone(),
        skip_silent_chunks: true,
    };
    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let runs = args.repeat.unwrap_or(1).max(1);
    let mut times_ms: Vec<u64> = Vec::with_capacity(runs);
    let mut last_output: Option<PipelineOutput> = None;
    for i in 0..runs {
        // The lease keeps the model loaded, but an engine failure can still
        // drop it; reload (untimed) so the timing below stays clean.
        if !tm.is_model_loaded() {
            if let Err(e) = tm.load_model_with_device(&model_id, device_index) {
                eprintln!("error: reload before run {} failed: {}", i + 1, e);
                return 1;
            }
        }
        let mut progress = Progress::new(show_progress);
        let started = Instant::now();
        let result = pipeline::run_pipeline(&tm, path, &options, &WorkToken::new(), &mut |event| {
            progress.on_event(&event)
        });
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(output) => {
                progress.finish(Some(output.transcript.duration_ms));
                times_ms.push(elapsed_ms);
                last_output = Some(output);
            }
            Err(e) => {
                progress.finish(None);
                match &e {
                    PipelineError::Cancelled { .. } => eprintln!("error: transcription cancelled"),
                    PipelineError::Failed { message, .. } => eprintln!("error: {}", message),
                }
                return e.exit_code();
            }
        }
    }
    drop(lease);
    // `runs` is at least 1, and every failed run returned above.
    let Some(mut output) = last_output else {
        return 1;
    };
    // As the app does: the Markdown header shows the model's name.
    output.transcript.model_name = app
        .try_state::<Arc<ModelManager>>()
        .and_then(|models| models.get_model_info(&model_id))
        .map(|info| info.name);

    for warning in &output.warnings {
        eprintln!("warning: {}", warning);
    }

    let report = RunReport {
        model_id: &model_id,
        requested_device: &requested_device,
        bound_backend: bound_backend.as_deref(),
        load_ms,
        times_ms: &times_ms,
        output: &output,
    };
    let transcript = &output.transcript;
    match &plan.destination {
        Destination::Legacy => {
            println!("{}", metrics_line(&report));
            println!(
                "text: {}",
                export::single_line(&transcript.text, transcript.language.as_deref())
            );
        }
        Destination::Json => println!("{}", legacy_json(&report, None)),
        Destination::Stdout(format) => {
            if !args.quiet {
                eprintln!("{}", metrics_line(&report));
            }
            // Rendered output already ends with a newline (or is empty).
            print!("{}", export::render(transcript, *format));
        }
        Destination::File {
            path: output_path,
            format,
            json,
        } => {
            if let Err(e) = export::write_atomic(output_path, &export::render(transcript, *format))
            {
                eprintln!("error: cannot write {}: {}", output_path.display(), e);
                return 1;
            }
            if *json {
                println!("{}", legacy_json(&report, Some(output_path)));
            } else {
                println!("{}", metrics_line(&report));
                println!("output: {}", output_path.display());
            }
        }
    }
    0
}

/// What `--transcribe-file` produces, decided from the flags alone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputPlan {
    destination: Destination,
    /// `--chunk-secs` in milliseconds; None = the model's default.
    max_chunk_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Destination {
    /// The metrics line and `text: <transcript on one line>` on stdout.
    Legacy,
    /// The metrics JSON object on stdout.
    Json,
    /// `--format` without `--output`: only the rendered transcript on stdout;
    /// the metrics line goes to stderr.
    Stdout(TranscriptFormat),
    /// `--output`: the rendered transcript goes to `path`, and stdout gets
    /// the metrics line and `output: <path>` (or the JSON with `"output"`).
    File {
        path: PathBuf,
        format: TranscriptFormat,
        json: bool,
    },
}

const JSON_WITH_FORMAT_ON_STDOUT: &str =
    "--format prints the transcript to stdout; add --output to combine it with --json";

/// Checks the flag combination and decides where the transcript goes. An
/// `Err` is a usage error (exit 2).
fn output_plan(args: &CliArgs) -> Result<OutputPlan, String> {
    let max_chunk_ms = match args.chunk_secs {
        None => None,
        Some(secs @ 1..=4) => {
            return Err(format!(
                "--chunk-secs must be 0 (whole file in one call) or at least 5, got {}",
                secs
            ))
        }
        Some(secs) => Some(
            secs.checked_mul(1000)
                .ok_or_else(|| format!("--chunk-secs {} is too large", secs))?,
        ),
    };

    let destination = match (&args.output, args.format) {
        (Some(path), format) => Destination::File {
            path: path.clone(),
            format: format
                .or_else(|| TranscriptFormat::from_path(path))
                .unwrap_or(TranscriptFormat::Txt),
            json: args.json,
        },
        (None, Some(_)) if args.json => return Err(JSON_WITH_FORMAT_ON_STDOUT.to_string()),
        (None, Some(format)) => Destination::Stdout(format),
        (None, None) if args.json => Destination::Json,
        (None, None) => Destination::Legacy,
    };

    Ok(OutputPlan {
        destination,
        max_chunk_ms,
    })
}

/// A probe error that names the input file once: `NotFound` messages
/// already contain its path.
fn probe_error(path: &Path, error: &DecodeError) -> String {
    match error {
        DecodeError::NotFound(_) => error.to_string(),
        _ => format!("{}: {}", path.display(), error),
    }
}

/// Catches an `--output` that can't work before the transcription runs: a
/// missing folder, a folder as the target, the input file itself, or a
/// place that can't be written.
fn check_output_path(output: &Path, input: &Path) -> Result<(), String> {
    let folder = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if !folder.is_dir() {
        return Err(format!("output folder not found: {}", folder.display()));
    }
    if output.is_dir() {
        return Err(format!("--output is a folder: {}", output.display()));
    }
    if let (Ok(output), Ok(input)) = (output.canonicalize(), input.canonicalize()) {
        if output == input {
            return Err("--output would overwrite the input file".to_string());
        }
    }
    export::check_writable(output).map_err(|e| format!("cannot write {}: {}", output.display(), e))
}

/// Warns when `--chunk-secs` asks for more audio per call than the model
/// handles (`hard_max_ms`); the run still goes ahead as asked.
fn chunk_warning(
    max_chunk_ms: Option<u32>,
    hard_max_ms: Option<u32>,
    duration_ms: Option<u64>,
) -> Option<String> {
    let hard_max_ms = hard_max_ms?;
    match max_chunk_ms? {
        0 if duration_ms.is_some_and(|d| d <= u64::from(hard_max_ms)) => None,
        0 => Some(format!(
            "--chunk-secs 0 sends the whole file in one call, but this model handles at most {} s; the transcript may be cut short or fail",
            hard_max_ms / 1000
        )),
        ms if ms > hard_max_ms => Some(format!(
            "--chunk-secs {} is longer than this model handles ({} s); chunks may be cut short or fail",
            ms / 1000,
            hard_max_ms / 1000
        )),
        _ => None,
    }
}

/// The metrics of a `--transcribe-file` command, as printed on stdout.
struct RunReport<'a> {
    model_id: &'a str,
    requested_device: &'a str,
    bound_backend: Option<&'a str>,
    load_ms: u64,
    /// Wall time of each `--repeat` run: decode, chunking and transcription.
    times_ms: &'a [u64],
    /// The result of the last run.
    output: &'a PipelineOutput,
}

impl RunReport<'_> {
    fn audio_secs(&self) -> f64 {
        self.output.transcript.duration_ms as f64 / 1000.0
    }

    fn best_ms(&self) -> u64 {
        self.times_ms.iter().copied().min().unwrap_or(0)
    }

    /// Seconds of audio transcribed per second of the fastest run.
    fn rtf(&self) -> f64 {
        let best_ms = self.best_ms();
        if best_ms > 0 {
            self.audio_secs() / (best_ms as f64 / 1000.0)
        } else {
            0.0
        }
    }
}

/// `model=… device=… backend=… audio=X.XXs load=Nms best=Nms rtf=X.XXx`
fn metrics_line(report: &RunReport) -> String {
    format!(
        "model={} device={} backend={} audio={:.2}s load={}ms best={}ms rtf={:.2}x",
        report.model_id,
        report.requested_device,
        report.bound_backend.unwrap_or("?"),
        report.audio_secs(),
        report.load_ms,
        report.best_ms(),
        report.rtf(),
    )
}

/// The `--json` object. The first nine keys predate file transcription and
/// keep their names and types; everything else is an addition.
fn legacy_json(report: &RunReport, output_path: Option<&Path>) -> serde_json::Value {
    let output = report.output;
    let transcript = &output.transcript;
    let mut json = serde_json::json!({
        "model": report.model_id,
        "requested_device": report.requested_device,
        "bound_backend": report.bound_backend,
        "audio_secs": report.audio_secs(),
        "load_ms": report.load_ms,
        "transcribe_ms": report.times_ms,
        "best_ms": report.best_ms(),
        "rtf": report.rtf(),
        "text": export::single_line(&transcript.text, transcript.language.as_deref()),

        "source": transcript.source_path,
        "decoder": output.decoder,
        "duration_ms": transcript.duration_ms,
        "chunks": output.chunks,
        "skipped_chunks": output.skipped_chunks,
        "failed_chunks": output.failed_chunks,
        "language": transcript.language,
        "timing": transcript.timing,
        "status": transcript.status,
        "warnings": output.warnings,
        "segments": transcript.segments,
    });
    if let (Some(path), Some(object)) = (output_path, json.as_object_mut()) {
        object.insert(
            "output".to_string(),
            path.to_string_lossy().into_owned().into(),
        );
    }
    json
}

/// One progress line, e.g. `[ 42%] 00:12:34 / 01:30:00  chunk 31`, or
/// `[  ?%] 00:12:34  chunk 31` when the duration is unknown.
fn progress_line(processed_ms: u64, duration_ms: Option<u64>, chunk: u32) -> String {
    let processed = export::format_clock(processed_ms);
    match duration_ms.filter(|&total| total > 0) {
        Some(total) => {
            let percent = u128::from(processed_ms.min(total)) * 100 / u128::from(total);
            format!(
                "[{:>3}%] {} / {}  chunk {}",
                percent,
                processed,
                export::format_clock(total),
                chunk
            )
        }
        None => format!("[  ?%] {}  chunk {}", processed, chunk),
    }
}

/// The progress line on stderr, rewritten in place with `\r` at most four
/// times a second. Does nothing when disabled (stderr is not a terminal, or
/// `--quiet`).
struct Progress {
    enabled: bool,
    duration_ms: Option<u64>,
    processed_ms: u64,
    /// 1-based number of the latest chunk.
    chunk: u32,
    last_draw: Option<Instant>,
}

impl Progress {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            duration_ms: None,
            processed_ms: 0,
            chunk: 0,
            last_draw: None,
        }
    }

    fn on_event(&mut self, event: &PipelineEvent) {
        match *event {
            PipelineEvent::Probed { duration_ms } => {
                self.duration_ms = duration_ms;
                return;
            }
            PipelineEvent::Transcribing {
                chunk_index,
                processed_ms,
            } => {
                self.chunk = chunk_index.saturating_add(1);
                self.processed_ms = processed_ms;
            }
            PipelineEvent::Chunk {
                index,
                processed_ms,
                ..
            } => {
                self.chunk = index.saturating_add(1);
                self.processed_ms = processed_ms;
            }
            PipelineEvent::ModelReady | PipelineEvent::Waiting | PipelineEvent::Speakers(_) => {
                return
            }
        }
        let now = Instant::now();
        if self.due(now) {
            self.draw(now);
        }
    }

    fn due(&self, now: Instant) -> bool {
        self.last_draw
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL)
    }

    fn draw(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.last_draw = Some(now);
        let line = progress_line(self.processed_ms, self.duration_ms, self.chunk);
        let mut stderr = std::io::stderr().lock();
        let _ = write!(stderr, "\r{}", line);
        let _ = stderr.flush();
    }

    /// Ends the progress line, if one was drawn. On success, `exact_ms` (the
    /// decoded length) is shown as complete first.
    fn finish(&mut self, exact_ms: Option<u64>) {
        if self.last_draw.is_none() {
            return;
        }
        if let Some(ms) = exact_ms {
            self.processed_ms = ms;
            self.duration_ms = Some(ms);
            self.draw(Instant::now());
        }
        eprintln!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::types::{
        DecoderKind, FileTranscript, TimingGrain, TranscriptSegment, TranscriptStatus,
    };
    use clap::Parser;

    fn args(flags: &[&str]) -> CliArgs {
        CliArgs::try_parse_from(
            ["handy", "-f", "talk.mp3"]
                .into_iter()
                .chain(flags.iter().copied()),
        )
        .unwrap()
    }

    fn plan(flags: &[&str]) -> Result<OutputPlan, String> {
        output_plan(&args(flags))
    }

    fn file(path: &str, format: TranscriptFormat, json: bool) -> Destination {
        Destination::File {
            path: PathBuf::from(path),
            format,
            json,
        }
    }

    #[test]
    fn output_plan_routes_every_flag_combination() {
        use TranscriptFormat::*;
        let cases: &[(&[&str], Destination)] = &[
            (&[], Destination::Legacy),
            (&["-q"], Destination::Legacy),
            (&["--json"], Destination::Json),
            (&["--format", "srt"], Destination::Stdout(Srt)),
            (&["--format", "markdown", "-q"], Destination::Stdout(Md)),
            (&["-o", "out.vtt"], file("out.vtt", Vtt, false)),
            (&["-o", "out.JSON"], file("out.JSON", Json, false)),
            (&["-o", "out.markdown"], file("out.markdown", Md, false)),
            (&["-o", "out"], file("out", Txt, false)),
            (&["-o", "out.docx"], file("out.docx", Txt, false)),
            (
                &["-o", "out.srt", "--format", "vtt"],
                file("out.srt", Vtt, false),
            ),
            (&["-o", "out.srt", "--json"], file("out.srt", Srt, true)),
            (
                &["-o", "out.txt", "--json", "--format", "json"],
                file("out.txt", Json, true),
            ),
        ];
        for (flags, destination) in cases {
            assert_eq!(
                plan(flags).map(|p| p.destination),
                Ok(destination.clone()),
                "{flags:?}"
            );
        }
    }

    #[test]
    fn output_plan_rejects_json_with_format_on_stdout() {
        let err = plan(&["--json", "--format", "srt"]).unwrap_err();
        assert_eq!(
            err,
            "--format prints the transcript to stdout; add --output to combine it with --json"
        );
    }

    #[test]
    fn output_plan_checks_chunk_secs() {
        assert_eq!(plan(&[]).unwrap().max_chunk_ms, None);
        assert_eq!(plan(&["--chunk-secs", "0"]).unwrap().max_chunk_ms, Some(0));
        assert_eq!(
            plan(&["--chunk-secs", "5"]).unwrap().max_chunk_ms,
            Some(5_000)
        );
        assert_eq!(
            plan(&["--chunk-secs", "120"]).unwrap().max_chunk_ms,
            Some(120_000)
        );
        for secs in ["1", "3", "4"] {
            let err = plan(&["--chunk-secs", secs]).unwrap_err();
            assert!(err.contains("at least 5"), "{err}");
        }
        assert!(plan(&["--chunk-secs", "5000000"]).is_err());
    }

    #[test]
    fn check_output_path_catches_unusable_targets() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("talk.wav");
        std::fs::write(&input, b"RIFF").unwrap();

        assert!(check_output_path(&dir.path().join("talk.srt"), &input).is_ok());
        assert!(check_output_path(Path::new("relative.txt"), &input).is_ok());
        // An existing transcript is replaced.
        let existing = input.with_extension("txt");
        std::fs::write(&existing, "old").unwrap();
        assert!(check_output_path(&existing, &input).is_ok());

        let err = check_output_path(&dir.path().join("missing/talk.srt"), &input).unwrap_err();
        assert!(err.contains("folder not found"), "{err}");
        let err = check_output_path(dir.path(), &input).unwrap_err();
        assert!(err.contains("is a folder"), "{err}");
        let err = check_output_path(&dir.path().join(".").join("talk.wav"), &input).unwrap_err();
        assert!(err.contains("overwrite the input"), "{err}");
        // The writability probe leaves nothing behind.
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["talk.txt", "talk.wav"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn check_output_path_catches_unwritable_folders_before_transcribing() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("talk.wav");
        std::fs::write(&input, b"RIFF").unwrap();

        // Exists, but no file can be created in it (not even by root).
        let err = check_output_path(Path::new("/proc/talk.srt"), &input).unwrap_err();
        assert!(err.starts_with("cannot write /proc/talk.srt: "), "{err}");
        // Devices are written to, not replaced, so they pass.
        assert!(check_output_path(Path::new("/dev/null"), &input).is_ok());
    }

    #[test]
    fn probe_errors_name_the_file_once() {
        let path = Path::new("/nope/x.mp3");
        assert_eq!(
            probe_error(
                path,
                &DecodeError::NotFound("/nope/x.mp3 does not exist".to_string())
            ),
            "/nope/x.mp3 does not exist"
        );
        assert_eq!(
            probe_error(
                path,
                &DecodeError::Unsupported("the file has no audio track".to_string())
            ),
            "/nope/x.mp3: unsupported file: the file has no audio track"
        );
        assert_eq!(
            probe_error(
                path,
                &DecodeError::NeedsFfmpeg("opus files need ffmpeg".into())
            ),
            "/nope/x.mp3: opus files need ffmpeg, but ffmpeg was not found"
        );
    }

    #[test]
    fn chunk_warning_only_when_over_the_model_limit() {
        assert_eq!(chunk_warning(None, Some(29_000), None), None);
        assert_eq!(chunk_warning(Some(60_000), None, None), None);
        assert_eq!(chunk_warning(Some(20_000), Some(29_000), None), None);
        assert_eq!(chunk_warning(Some(29_000), Some(29_000), None), None);
        let warning = chunk_warning(Some(60_000), Some(29_000), None).unwrap();
        assert!(
            warning.starts_with("--chunk-secs 60 is longer"),
            "{warning}"
        );
        assert!(warning.contains("(29 s)"), "{warning}");
        // Whole file in one call: fine if the file is short enough.
        assert_eq!(chunk_warning(Some(0), Some(29_000), Some(20_000)), None);
        assert!(chunk_warning(Some(0), Some(29_000), Some(45_000)).is_some());
        assert!(chunk_warning(Some(0), Some(29_000), None).is_some());
        assert_eq!(chunk_warning(Some(0), None, Some(7_200_000)), None);
    }

    fn sample_output() -> PipelineOutput {
        PipelineOutput {
            transcript: FileTranscript {
                source_path: "/tmp/talk.mp3".to_string(),
                source_name: "talk.mp3".to_string(),
                duration_ms: 3_725_000,
                model_id: "small".to_string(),
                model_name: None,
                language: Some("en".to_string()),
                status: TranscriptStatus::Partial,
                status_detail: Some("Could not transcribe 00:41:10–00:41:38: boom".to_string()),
                timing: TimingGrain::Segment,
                text: "First paragraph.\n\nSecond paragraph.".to_string(),
                paragraphs: Vec::new(),
                segments: vec![
                    TranscriptSegment::new(0, 2_500, "First paragraph."),
                    TranscriptSegment::new(28_000, 30_000, "Second paragraph."),
                ],
                created_at: 1_700_000_000,
                history_id: None,
                speakers: None,
            },
            decoder: DecoderKind::Symphonia,
            chunks: 134,
            skipped_chunks: 3,
            failed_chunks: 1,
            warnings: vec!["Could not transcribe 00:41:10–00:41:38: boom".to_string()],
            source_size_bytes: 1_234,
            source_modified_at: Some(1_699_000_000),
        }
    }

    fn report<'a>(
        output: &'a PipelineOutput,
        backend: Option<&'a str>,
        times_ms: &'a [u64],
    ) -> RunReport<'a> {
        RunReport {
            model_id: "small",
            requested_device: "settings",
            bound_backend: backend,
            load_ms: 1_200,
            times_ms,
            output,
        }
    }

    #[test]
    fn metrics_line_keeps_the_legacy_format() {
        let output = sample_output();
        assert_eq!(
            metrics_line(&report(&output, Some("cpu"), &[60_000, 50_000])),
            "model=small device=settings backend=cpu audio=3725.00s load=1200ms best=50000ms rtf=74.50x"
        );
        assert_eq!(
            metrics_line(&report(&output, None, &[0])),
            "model=small device=settings backend=? audio=3725.00s load=1200ms best=0ms rtf=0.00x"
        );
    }

    #[test]
    fn metrics_of_an_empty_recording() {
        let mut output = sample_output();
        output.transcript.duration_ms = 0;
        output.transcript.text = String::new();
        output.transcript.status = TranscriptStatus::Completed;
        assert_eq!(
            metrics_line(&report(&output, Some("cpu"), &[12])),
            "model=small device=settings backend=cpu audio=0.00s load=1200ms best=12ms rtf=0.00x"
        );
        let json = legacy_json(&report(&output, Some("cpu"), &[12]), None);
        assert_eq!(json["audio_secs"], 0.0);
        assert_eq!(json["text"], "");
    }

    #[test]
    fn legacy_json_keeps_the_nine_keys_and_adds_file_fields() {
        let output = sample_output();
        let json = legacy_json(&report(&output, Some("cpu"), &[60_000, 50_000]), None);

        // The original keys, with their original types.
        assert_eq!(json["model"], "small");
        assert_eq!(json["requested_device"], "settings");
        assert_eq!(json["bound_backend"], "cpu");
        assert!(json["audio_secs"].is_f64());
        assert_eq!(json["audio_secs"], 3725.0);
        assert!(json["load_ms"].is_u64());
        assert_eq!(json["load_ms"], 1_200);
        assert_eq!(json["transcribe_ms"], serde_json::json!([60_000, 50_000]));
        assert!(json["transcribe_ms"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.is_u64()));
        assert!(json["best_ms"].is_u64());
        assert_eq!(json["best_ms"], 50_000);
        assert!(json["rtf"].is_f64());
        assert_eq!(json["rtf"], 74.5);
        assert_eq!(json["text"], "First paragraph. Second paragraph.");

        // Additions.
        assert_eq!(json["source"], "/tmp/talk.mp3");
        assert_eq!(json["decoder"], "symphonia");
        assert_eq!(json["duration_ms"], 3_725_000);
        assert_eq!(json["chunks"], 134);
        assert_eq!(json["skipped_chunks"], 3);
        assert_eq!(json["failed_chunks"], 1);
        assert_eq!(json["language"], "en");
        assert_eq!(json["timing"], "segment");
        assert_eq!(json["status"], "partial");
        assert_eq!(
            json["warnings"],
            serde_json::json!(["Could not transcribe 00:41:10–00:41:38: boom"])
        );
        assert_eq!(
            json["segments"],
            serde_json::json!([
                {"start_ms": 0, "end_ms": 2_500, "text": "First paragraph."},
                {"start_ms": 28_000, "end_ms": 30_000, "text": "Second paragraph."},
            ])
        );
        assert!(json.get("output").is_none());
        assert_eq!(json.as_object().unwrap().len(), 20);
    }

    #[test]
    fn legacy_json_null_backend_unknown_language_and_output_path() {
        let mut output = sample_output();
        output.transcript.language = None;
        output.transcript.status = TranscriptStatus::Completed;
        output.transcript.timing = TimingGrain::Chunk;
        let json = legacy_json(
            &report(&output, None, &[0]),
            Some(Path::new("/tmp/talk.srt")),
        );
        assert!(json["bound_backend"].is_null());
        assert!(json["language"].is_null());
        assert_eq!(json["timing"], "chunk");
        assert_eq!(json["status"], "completed");
        assert!(json["rtf"].is_f64());
        assert_eq!(json["output"], "/tmp/talk.srt");
    }

    #[test]
    fn progress_lines() {
        assert_eq!(
            progress_line(754_000, Some(5_400_000), 31),
            "[ 13%] 00:12:34 / 01:30:00  chunk 31"
        );
        assert_eq!(
            progress_line(0, Some(60_000), 1),
            "[  0%] 00:00:00 / 00:01:00  chunk 1"
        );
        assert_eq!(
            progress_line(60_000, Some(60_000), 3),
            "[100%] 00:01:00 / 00:01:00  chunk 3"
        );
        // The probed duration is an estimate: never show more than 100%.
        assert_eq!(
            progress_line(61_000, Some(60_000), 3),
            "[100%] 00:01:01 / 00:01:00  chunk 3"
        );
        assert_eq!(
            progress_line(754_000, None, 31),
            "[  ?%] 00:12:34  chunk 31"
        );
        assert_eq!(
            progress_line(754_000, Some(0), 31),
            "[  ?%] 00:12:34  chunk 31"
        );
    }

    #[test]
    fn progress_redraws_at_most_four_times_a_second() {
        let mut progress = Progress::new(false);
        let start = Instant::now();
        assert!(progress.due(start));
        progress.last_draw = Some(start);
        assert!(!progress.due(start + Duration::from_millis(249)));
        assert!(progress.due(start + Duration::from_millis(250)));
    }

    #[test]
    fn progress_tracks_pipeline_events() {
        let mut progress = Progress::new(false);
        progress.on_event(&PipelineEvent::Probed {
            duration_ms: Some(90_000),
        });
        progress.on_event(&PipelineEvent::ModelReady);
        progress.on_event(&PipelineEvent::Transcribing {
            chunk_index: 0,
            processed_ms: 0,
        });
        assert_eq!((progress.chunk, progress.processed_ms), (1, 0));
        progress.on_event(&PipelineEvent::Chunk {
            index: 0,
            paragraph: None,
            segments: Vec::new(),
            language: None,
            processed_ms: 27_500,
            skipped: true,
        });
        progress.on_event(&PipelineEvent::Waiting);
        assert_eq!((progress.chunk, progress.processed_ms), (1, 27_500));
        assert_eq!(progress.duration_ms, Some(90_000));
        // Disabled: nothing is drawn, so finishing prints nothing either.
        assert!(progress.last_draw.is_none());
    }
}
