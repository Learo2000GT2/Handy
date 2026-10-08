//! The headless CLI path (`--transcribe-file`, `--list-devices`,
//! `--list-models`, `--list-lemonade-models`), run on a worker thread when
//! Handy starts in headless mode.

use crate::cli::CliArgs;
use crate::engine_supervisor::WorkToken;
use crate::file_transcription::decode::{self, DecodeError, DecodeOptions};
use crate::file_transcription::export;
use crate::file_transcription::lemonade::{
    self, LemonadeClient, LemonadeError, LemonadeJobConfig, LemonadeTimeouts, LemonadeTranscriber,
    LEMONADE_MODEL_PREFIX,
};
use crate::file_transcription::pipeline::{
    self, PipelineEngine, PipelineError, PipelineEvent, PipelineOptions, PipelineOutput,
};
use crate::file_transcription::speaker_model;
use crate::file_transcription::types::{SpeakerJobState, SpeakerSkipReason, TranscriptFormat};
use crate::managers::model::ModelManager;
use crate::managers::transcription::{FileJobLease, TranscriptionManager};
use crate::settings::{
    get_settings, AppSettings, FileTranscriptionEngine, LemonadeSettings, DEFAULT_LEMONADE_URL,
};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// Shortest time between two redraws of the progress line.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// Environment variable with the Lemonade API key (safer than --lemonade-key,
/// which other processes can see).
const LEMONADE_KEY_ENV: &str = "HANDY_LEMONADE_API_KEY";

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

    // --list-lemonade-models: the transcription models of the Lemonade
    // Server (--lemonade-url/--lemonade-key, else the app's settings).
    if args.list_lemonade_models {
        let code = list_lemonade_models(app, args);
        if code != 0 || args.transcribe_file.is_none() {
            return code;
        }
    }

    match args.transcribe_file.as_deref() {
        Some(path) => transcribe_file(app, args, path),
        None => 0,
    }
}

/// `--list-lemonade-models`: prints the server's transcription models (or,
/// with `--json`, them as a JSON array). Returns the exit code.
fn list_lemonade_models(app: &AppHandle, args: &CliArgs) -> i32 {
    let settings = get_settings(app);
    let server = lemonade_server(args, &settings.lemonade, lemonade_key_from_env());
    let client = match LemonadeClient::new(
        &server.base_url,
        server.api_key.as_deref(),
        LemonadeTimeouts::default(),
    ) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    let models = match client.list_models_blocking(&AtomicBool::new(false)) {
        Ok(models) => models,
        Err(e) => {
            eprintln!("error: {}", e);
            return match e {
                LemonadeError::InvalidUrl(_) | LemonadeError::Unauthorized { .. } => 2,
                _ => 1,
            };
        }
    };
    let (models, others): (Vec<_>, Vec<_>) = models
        .into_iter()
        .partition(lemonade::LemonadeModel::is_transcription);
    if args.json {
        match serde_json::to_string_pretty(&models) {
            Ok(s) => println!("{}", s),
            Err(e) => {
                eprintln!("error: failed to serialize models: {}", e);
                return 1;
            }
        }
        return 0;
    }
    if models.is_empty() {
        println!(
            "No transcription models on the Lemonade Server at {}.",
            client.api_url()
        );
    } else {
        println!(
            "Transcription models on {} (✓ = downloaded, ? = unknown):",
            client.api_url()
        );
        let width = models.iter().map(|m| m.id.len()).max().unwrap_or(0);
        for m in &models {
            let mark = match m.downloaded {
                Some(true) => "✓",
                Some(false) => " ",
                None => "?",
            };
            println!(
                "  {}  {:<width$}  {}",
                mark,
                m.id,
                m.recipe.as_deref().unwrap_or(""),
                width = width
            );
        }
    }
    if !others.is_empty() {
        println!("({} other models not shown)", others.len());
    }
    0
}

/// The Lemonade API key from the environment, if set and not blank.
fn lemonade_key_from_env() -> Option<String> {
    std::env::var(LEMONADE_KEY_ENV)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

/// The Lemonade Server to use: each value from its flag, else (the key) the
/// environment, else the app's setting, else the default address.
fn lemonade_server(
    args: &CliArgs,
    settings: &LemonadeSettings,
    env_key: Option<String>,
) -> LemonadeJobConfig {
    let base_url = args
        .lemonade_url
        .clone()
        .filter(|url| !url.trim().is_empty())
        .or_else(|| Some(settings.base_url.clone()).filter(|url| !url.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_LEMONADE_URL.to_string());
    LemonadeJobConfig::from_settings(&LemonadeSettings {
        base_url,
        model: args
            .lemonade_model
            .clone()
            .unwrap_or_else(|| settings.model.clone()),
        api_key: args
            .lemonade_key
            .clone()
            .or(env_key)
            .unwrap_or_else(|| settings.api_key.clone()),
    })
}

/// The engine `--transcribe-file` runs on.
#[derive(Debug, Clone)]
enum CliEngine {
    Handy {
        model_id: String,
        device_index: Option<usize>,
    },
    Lemonade(LemonadeJobConfig),
}

impl CliEngine {
    fn kind(&self) -> FileTranscriptionEngine {
        match self {
            Self::Handy { .. } => FileTranscriptionEngine::Handy,
            Self::Lemonade(_) => FileTranscriptionEngine::Lemonade,
        }
    }
}

/// Picks the engine from the flags and settings. Returns it with warnings
/// to print; an `Err` is a usage error (exit 2).
fn cli_engine(
    args: &CliArgs,
    settings: &AppSettings,
    env_key: Option<String>,
) -> Result<(CliEngine, Vec<String>), String> {
    let mut warnings = Vec::new();
    // --model and --device-index pick one of Handy's models, so they imply
    // the Handy engine unless --engine says otherwise.
    let picks_handy_model = args.model.is_some() || args.device_index.is_some();
    let engine = args.engine.unwrap_or(if picks_handy_model {
        FileTranscriptionEngine::Handy
    } else {
        settings.file_transcription_engine
    });
    match engine {
        FileTranscriptionEngine::Handy => {
            if args.lemonade_url.is_some()
                || args.lemonade_model.is_some()
                || args.lemonade_key.is_some()
            {
                warnings.push(
                    "--lemonade-url, --lemonade-model and --lemonade-key only apply to --engine lemonade; ignored"
                        .to_string(),
                );
            }
            let model_id = args
                .model
                .clone()
                .unwrap_or_else(|| settings.selected_model.clone());
            if model_id.is_empty() {
                return Err("no model selected (pass --model or pick one in the app)".to_string());
            }
            Ok((
                CliEngine::Handy {
                    model_id,
                    device_index: args.device_index,
                },
                warnings,
            ))
        }
        FileTranscriptionEngine::Lemonade => {
            if picks_handy_model {
                return Err(
                    "--model and --device-index choose Handy's model; they can't be used with --engine lemonade"
                        .to_string(),
                );
            }
            let config = lemonade_server(args, &settings.lemonade, env_key);
            if config.model.is_empty() {
                return Err(
                    "no Lemonade model (pass --lemonade-model or pick one in the app)".to_string(),
                );
            }
            if lemonade::normalize_base_url(&config.base_url).is_err() {
                return Err(format!(
                    "invalid Lemonade Server address: {} (expected something like {})",
                    lemonade::redact_url(&config.base_url),
                    DEFAULT_LEMONADE_URL
                ));
            }
            Ok((CliEngine::Lemonade(config), warnings))
        }
    }
}

/// The Sortformer model for `--speakers`: `--speaker-model`, else the one the
/// app downloaded. None without `--speakers`; an `Err` is a usage error.
fn cli_speaker_model(
    args: &CliArgs,
    installed: impl FnOnce() -> Option<PathBuf>,
) -> Result<Option<PathBuf>, String> {
    if !args.speakers {
        return Ok(None);
    }
    match args.speaker_model.clone() {
        Some(path) if path.is_file() => Ok(Some(path)),
        Some(path) => Err(format!("speaker model not found: {}", path.display())),
        None => installed().map(Some).ok_or_else(|| {
            "the speaker model isn't installed (enable Label speakers in the app, or pass --speaker-model)"
                .to_string()
        }),
    }
}

/// Why speaker labels were not made, for a `warning:` line; None when they
/// were (or a failure was already reported among the run's warnings).
fn speaker_skip_warning(state: Option<&SpeakerJobState>) -> Option<String> {
    let reason = match state? {
        SpeakerJobState::Skipped { reason } => reason,
        _ => return None,
    };
    Some(
        match reason {
            SpeakerSkipReason::ModelMissing => {
                "speaker labels skipped: the speaker model is missing"
            }
            SpeakerSkipReason::NoSpeech => "speaker labels skipped: no speech found",
        }
        .to_string(),
    )
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

    let settings = get_settings(app);
    let (engine, engine_warnings) = match cli_engine(args, &settings, lemonade_key_from_env()) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("error: {}", e);
            return 2;
        }
    };
    for warning in &engine_warnings {
        eprintln!("warning: {}", warning);
    }
    let speaker_model =
        match cli_speaker_model(args, || speaker_model::installed_speaker_model(app)) {
            Ok(path) => path,
            Err(e) => {
                eprintln!("error: {}", e);
                return 2;
            }
        };

    let engine_kind = engine.kind();
    let tm = app.state::<Arc<TranscriptionManager>>();
    let prepared = match prepare_engine(&tm, engine, &settings, &plan, probe.duration_ms) {
        Ok(prepared) => prepared,
        Err(code) => return code,
    };
    let PreparedEngine {
        model_id,
        requested_device,
        load_ms,
        bound_backend,
        lease,
        engine,
    } = prepared;

    let options = PipelineOptions {
        engine,
        max_chunk_ms: plan.max_chunk_ms,
        ffmpeg: args.ffmpeg.clone(),
        skip_silent_chunks: true,
        speaker_model,
    };
    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let runs = args.repeat.unwrap_or(1).max(1);
    let mut times_ms: Vec<u64> = Vec::with_capacity(runs);
    let mut last_output: Option<PipelineOutput> = None;
    for i in 0..runs {
        // The lease keeps the model loaded, but an engine failure can still
        // drop it; reload (untimed) so the timing below stays clean.
        if let PipelineEngine::Handy { device_index, .. } = &options.engine {
            if !tm.is_model_loaded() {
                if let Err(e) = tm.load_model_with_device(&model_id, *device_index) {
                    eprintln!("error: reload before run {} failed: {}", i + 1, e);
                    return 1;
                }
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
    output.transcript.model_name = lemonade::display_model_name(&model_id).or_else(|| {
        app.try_state::<Arc<ModelManager>>()
            .and_then(|models| models.get_model_info(&model_id))
            .map(|info| info.name)
    });

    for warning in &output.warnings {
        eprintln!("warning: {}", warning);
    }
    if let Some(warning) = speaker_skip_warning(output.speakers.as_ref()) {
        eprintln!("warning: {}", warning);
    }

    let report = RunReport {
        engine: engine_kind,
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

/// An engine ready for `--transcribe-file`'s runs, with what the metrics
/// report about it.
struct PreparedEngine {
    /// "small", or "lemonade:Whisper-Base".
    model_id: String,
    requested_device: String,
    /// The model's cold load, or Lemonade's preflight.
    load_ms: u64,
    bound_backend: Option<String>,
    /// Holds Handy's model for the whole command; None for Lemonade.
    lease: Option<FileJobLease>,
    engine: PipelineEngine,
}

/// Loads Handy's model (timed) and leases it, or checks the Lemonade Server
/// (timed preflight). Prints the error and returns the exit code on failure.
fn prepare_engine(
    tm: &TranscriptionManager,
    engine: CliEngine,
    settings: &AppSettings,
    plan: &OutputPlan,
    duration_ms: Option<u64>,
) -> Result<PreparedEngine, i32> {
    match engine {
        CliEngine::Handy {
            model_id,
            device_index,
        } => {
            // --device-index hard-selects a compute device by its --list-devices registry
            // index (transcribe-cpp / whisper-family models only; not persisted). Omit it
            // to use the persisted accelerator setting.
            let requested_device = match device_index {
                Some(idx) => format!("index {}", idx),
                None => "settings".to_string(),
            };

            // Cold load (timed).
            let load_start = Instant::now();
            if let Err(e) = tm.load_model_with_device(&model_id, device_index) {
                eprintln!("error: load_model('{}') failed: {}", model_id, e);
                return Err(1);
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
                    return Err(1);
                }
            };
            if let Some(warning) =
                chunk_warning(plan.max_chunk_ms, lease.limits().hard_max_ms, duration_ms)
            {
                eprintln!("warning: {}", warning);
            }
            Ok(PreparedEngine {
                engine: PipelineEngine::Handy {
                    model_id: model_id.clone(),
                    device_index,
                },
                model_id,
                requested_device,
                load_ms,
                bound_backend,
                lease: Some(lease),
            })
        }
        CliEngine::Lemonade(config) => {
            // None of Handy's models is loaded: the preflight (server, key
            // and model list) stands in for the load, and fails fast.
            let load_start = Instant::now();
            if let Err(e) = LemonadeTranscriber::begin(&config, settings.clone(), WorkToken::new())
            {
                let e = pipeline::engine_start_error(e);
                eprintln!("error: {}", e);
                return Err(e.exit_code());
            }
            let load_ms = load_start.elapsed().as_millis() as u64;
            Ok(PreparedEngine {
                model_id: format!("{}{}", LEMONADE_MODEL_PREFIX, config.model),
                requested_device: "server".to_string(),
                load_ms,
                bound_backend: Some("lemonade".to_string()),
                lease: None,
                engine: PipelineEngine::Lemonade {
                    config,
                    settings: settings.clone(),
                },
            })
        }
    }
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
    engine: FileTranscriptionEngine,
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
        "engine": report.engine,
        "speakers": transcript.speakers,
        "speaker_state": output.speakers,
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
        DecoderKind, FileTranscript, TimingGrain, TranscriptSegment, TranscriptSpeaker,
        TranscriptSpeakers, TranscriptStatus,
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
            speakers: None,
        }
    }

    fn report<'a>(
        output: &'a PipelineOutput,
        backend: Option<&'a str>,
        times_ms: &'a [u64],
    ) -> RunReport<'a> {
        RunReport {
            engine: FileTranscriptionEngine::Handy,
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

        // Engine and speaker labels.
        assert_eq!(json["engine"], "handy");
        assert!(json["speakers"].is_null());
        assert!(json["speaker_state"].is_null());
        assert_eq!(json.as_object().unwrap().len(), 23);
    }

    #[test]
    fn legacy_json_reports_lemonade_and_speaker_labels() {
        let mut output = sample_output();
        output.transcript.segments[1].speaker = Some(2);
        output.transcript.speakers = Some(TranscriptSpeakers {
            speakers: vec![
                TranscriptSpeaker { id: 1, name: None },
                TranscriptSpeaker {
                    id: 2,
                    name: Some("Doug".to_string()),
                },
            ],
            approximate: false,
            unassigned_segments: 0,
            model: "sortformer-4spk-v2.1-q8_0".to_string(),
        });
        output.speakers = Some(SpeakerJobState::Done { speakers: 2 });
        let mut report = report(&output, Some("lemonade"), &[1_000]);
        report.engine = FileTranscriptionEngine::Lemonade;
        report.model_id = "lemonade:Whisper-Tiny";
        report.requested_device = "server";

        let json = legacy_json(&report, None);

        assert_eq!(json["engine"], "lemonade");
        assert_eq!(json["model"], "lemonade:Whisper-Tiny");
        assert_eq!(json["bound_backend"], "lemonade");
        assert_eq!(json["speakers"]["speakers"][1]["name"], "Doug");
        assert_eq!(json["speakers"]["model"], "sortformer-4spk-v2.1-q8_0");
        assert_eq!(
            json["speaker_state"],
            serde_json::json!({"state": "done", "speakers": 2})
        );
        assert!(json["segments"][0].get("speaker").is_none());
        assert_eq!(json["segments"][1]["speaker"], 2);
        assert_eq!(
            metrics_line(&report),
            "model=lemonade:Whisper-Tiny device=server backend=lemonade audio=3725.00s load=1200ms best=1000ms rtf=3725.00x"
        );
    }

    fn settings_with(engine: FileTranscriptionEngine) -> AppSettings {
        let mut settings = crate::settings::get_default_settings();
        settings.selected_model = "small".to_string();
        settings.file_transcription_engine = engine;
        settings.lemonade = LemonadeSettings {
            base_url: "http://lemon.local:8000/api/v1".to_string(),
            model: "Whisper-Base".to_string(),
            api_key: "from-settings".to_string(),
        };
        settings
    }

    fn lemonade_config(engine: CliEngine) -> LemonadeJobConfig {
        match engine {
            CliEngine::Lemonade(config) => config,
            CliEngine::Handy { .. } => panic!("expected Lemonade, got {engine:?}"),
        }
    }

    #[test]
    fn engine_follows_the_flag_then_the_setting() {
        let handy = settings_with(FileTranscriptionEngine::Handy);
        let lemon = settings_with(FileTranscriptionEngine::Lemonade);

        let (engine, warnings) = cli_engine(&args(&[]), &handy, None).unwrap();
        assert!(warnings.is_empty());
        assert!(matches!(
            engine,
            CliEngine::Handy { ref model_id, device_index: None } if model_id == "small"
        ));
        let (engine, _) = cli_engine(&args(&[]), &lemon, None).unwrap();
        assert_eq!(engine.kind(), FileTranscriptionEngine::Lemonade);
        let (engine, _) = cli_engine(&args(&["--engine", "handy"]), &lemon, None).unwrap();
        assert_eq!(engine.kind(), FileTranscriptionEngine::Handy);
        let (engine, _) = cli_engine(&args(&["--engine", "lemonade"]), &handy, None).unwrap();
        assert_eq!(engine.kind(), FileTranscriptionEngine::Lemonade);

        // Choosing one of Handy's models implies Handy's engine.
        let (engine, _) = cli_engine(&args(&["--model", "tiny"]), &lemon, None).unwrap();
        assert!(matches!(
            engine,
            CliEngine::Handy { ref model_id, .. } if model_id == "tiny"
        ));
        let (engine, _) = cli_engine(&args(&["--device-index", "1"]), &lemon, None).unwrap();
        assert!(matches!(
            engine,
            CliEngine::Handy {
                device_index: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn engine_usage_errors_and_warnings() {
        let handy = settings_with(FileTranscriptionEngine::Handy);
        for flags in [
            &["--engine", "lemonade", "--model", "tiny"][..],
            &["--engine", "lemonade", "--device-index", "0"][..],
        ] {
            let err = cli_engine(&args(flags), &handy, None).unwrap_err();
            assert!(
                err.contains("can't be used with --engine lemonade"),
                "{err}"
            );
        }

        let mut no_model = settings_with(FileTranscriptionEngine::Lemonade);
        no_model.lemonade.model = String::new();
        assert_eq!(
            cli_engine(&args(&[]), &no_model, None).unwrap_err(),
            "no Lemonade model (pass --lemonade-model or pick one in the app)"
        );
        let err = cli_engine(
            &args(&["--engine", "lemonade", "--lemonade-url", "localhost:13305"]),
            &handy,
            None,
        )
        .unwrap_err();
        assert!(err.starts_with("invalid Lemonade Server address"), "{err}");
        // A password in the address is not echoed.
        let err = cli_engine(
            &args(&[
                "--engine",
                "lemonade",
                "--lemonade-url",
                "http://alice:s3cretpw@lemon.lan:13305/?x=1",
            ]),
            &handy,
            None,
        )
        .unwrap_err();
        assert!(err.contains("http://lemon.lan:13305/?x=1"), "{err}");
        assert!(!err.contains("s3cretpw"), "{err}");

        let mut unselected = settings_with(FileTranscriptionEngine::Handy);
        unselected.selected_model = String::new();
        assert_eq!(
            cli_engine(&args(&[]), &unselected, None).unwrap_err(),
            "no model selected (pass --model or pick one in the app)"
        );

        let (_, warnings) =
            cli_engine(&args(&["--lemonade-model", "Whisper-Tiny"]), &handy, None).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("only apply to --engine lemonade"));
    }

    #[test]
    fn lemonade_values_come_from_flags_then_environment_then_settings() {
        let settings = settings_with(FileTranscriptionEngine::Lemonade);

        let config = lemonade_config(cli_engine(&args(&[]), &settings, None).unwrap().0);
        assert_eq!(config.base_url, "http://lemon.local:8000/api/v1");
        assert_eq!(config.model, "Whisper-Base");
        assert_eq!(config.api_key.as_deref(), Some("from-settings"));

        let env = Some("from-env".to_string());
        let config = lemonade_config(cli_engine(&args(&[]), &settings, env.clone()).unwrap().0);
        assert_eq!(config.api_key.as_deref(), Some("from-env"));

        let flags = [
            "--lemonade-url",
            " http://127.0.0.1:13399 ",
            "--lemonade-model",
            "Whisper-Tiny",
            "--lemonade-key",
            "from-flag",
        ];
        let config = lemonade_config(cli_engine(&args(&flags), &settings, env).unwrap().0);
        assert_eq!(config.base_url, "http://127.0.0.1:13399");
        assert_eq!(config.model, "Whisper-Tiny");
        assert_eq!(config.api_key.as_deref(), Some("from-flag"));

        // No address anywhere: the default one; a blank key is no key.
        let mut blank = settings_with(FileTranscriptionEngine::Lemonade);
        blank.lemonade.base_url = " ".to_string();
        blank.lemonade.api_key = String::new();
        let config = lemonade_config(cli_engine(&args(&[]), &blank, None).unwrap().0);
        assert_eq!(config.base_url, DEFAULT_LEMONADE_URL);
        assert_eq!(config.api_key, None);
    }

    #[test]
    fn speaker_model_comes_from_the_flag_or_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("sortformer.gguf");
        std::fs::write(&model, b"GGUF").unwrap();
        let installed = || Some(PathBuf::from("/app/speaker-models/x.gguf"));

        assert_eq!(cli_speaker_model(&args(&[]), installed), Ok(None));
        assert_eq!(
            cli_speaker_model(&args(&["--speakers"]), installed),
            Ok(installed())
        );
        let flag = model.to_string_lossy().into_owned();
        assert_eq!(
            cli_speaker_model(&args(&["--speakers", "--speaker-model", &flag]), || None),
            Ok(Some(model.clone()))
        );
        let err = cli_speaker_model(&args(&["--speakers"]), || None).unwrap_err();
        assert!(
            err.starts_with("the speaker model isn't installed"),
            "{err}"
        );
        let missing = dir
            .path()
            .join("missing.gguf")
            .to_string_lossy()
            .into_owned();
        let err = cli_speaker_model(
            &args(&["--speakers", "--speaker-model", &missing]),
            installed,
        )
        .unwrap_err();
        assert!(err.starts_with("speaker model not found: "), "{err}");
    }

    #[test]
    fn only_skipped_speaker_labels_add_a_warning() {
        assert_eq!(speaker_skip_warning(None), None);
        assert_eq!(
            speaker_skip_warning(Some(&SpeakerJobState::Done { speakers: 2 })),
            None
        );
        // Failures are already among the run's warnings.
        assert_eq!(
            speaker_skip_warning(Some(&SpeakerJobState::Failed {
                message: "boom".to_string()
            })),
            None
        );
        assert_eq!(
            speaker_skip_warning(Some(&SpeakerJobState::Skipped {
                reason: SpeakerSkipReason::NoSpeech
            }))
            .as_deref(),
            Some("speaker labels skipped: no speech found")
        );
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
