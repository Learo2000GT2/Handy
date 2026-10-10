use crate::file_transcription::types::TranscriptFormat;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone, Default)]
#[command(name = "handy", about = "Handy - Speech to Text")]
#[command(group(
    clap::ArgGroup::new("engine_use")
        .args(["transcribe_file", "list_lemonade_models"])
        .multiple(true)
))]
pub struct CliArgs {
    /// Start with the main window hidden
    #[arg(long)]
    pub start_hidden: bool,

    /// Disable the system tray icon
    #[arg(long)]
    pub no_tray: bool,

    /// Toggle transcription on/off (sent to running instance)
    #[arg(long)]
    pub toggle_transcription: bool,

    /// Toggle transcription with post-processing on/off (sent to running instance)
    #[arg(long)]
    pub toggle_post_process: bool,

    /// Cancel the current operation (sent to running instance)
    #[arg(long)]
    pub cancel: bool,

    /// Enable debug mode with verbose logging
    #[arg(long)]
    pub debug: bool,

    /// Transcribe this audio or video file headlessly and exit. Reads WAV,
    /// AIFF, MP3, FLAC, Ogg Vorbis, M4A/AAC and MP4/MOV audio natively, and
    /// formats like Opus, WebM, WMA, AMR and DSS when ffmpeg is installed.
    /// Long recordings are split into chunks. Same transcription path as the
    /// app — no mic, no download (the model must already be installed).
    #[arg(short = 'f', long, value_name = "FILE")]
    pub transcribe_file: Option<PathBuf>,

    /// Model id to load for --transcribe-file (default: the selected model).
    #[arg(long)]
    pub model: Option<String>,

    /// Hard-select the compute device for --transcribe-file by its registry
    /// index (see --list-devices). Omit to use the persisted accelerator
    /// setting. transcribe-cpp (whisper-family) models only.
    #[arg(long, value_name = "N")]
    pub device_index: Option<usize>,

    /// List the transcribe-cpp compute devices (with indices) and exit.
    #[arg(long)]
    pub list_devices: bool,

    /// List the available models (with ids) and exit. Pass an id to --model.
    /// Honors --json for machine-readable output.
    #[arg(long)]
    pub list_models: bool,

    /// Repeat the transcription N times (best_ms reports the fastest run).
    #[arg(long, value_name = "N")]
    pub repeat: Option<usize>,

    /// Emit --transcribe-file results as JSON.
    #[arg(long)]
    pub json: bool,

    /// Write the transcript to this file instead of printing it. Format:
    /// --format, else the file extension, else plain text.
    #[arg(short = 'o', long, value_name = "PATH", requires = "transcribe_file")]
    pub output: Option<PathBuf>,

    /// Transcript format for --output or stdout: txt, md, srt, vtt, json or
    /// docx (docx needs --output).
    #[arg(long, value_enum, value_name = "FORMAT", requires = "transcribe_file")]
    pub format: Option<TranscriptFormat>,

    /// Longest chunk sent to the model, in seconds: 0 = whole file in one
    /// call, otherwise at least 5. Default: the model's limit (at most 28).
    #[arg(long, value_name = "SECS", requires = "transcribe_file")]
    pub chunk_secs: Option<u32>,

    /// ffmpeg binary for formats the built-in decoder can't read.
    /// Default: $HANDY_FFMPEG, then PATH and common install folders.
    #[arg(long, value_name = "PATH", requires = "transcribe_file")]
    pub ffmpeg: Option<PathBuf>,

    /// Don't print progress (or, with --format, the metrics line) to stderr.
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Engine for --transcribe-file: handy (the selected model) or lemonade
    /// (Lemonade Server). Default: the app's setting.
    #[arg(long, value_enum, value_name = "ENGINE", requires = "engine_use")]
    pub engine: Option<crate::settings::FileTranscriptionEngine>,

    /// Lemonade Server address (default: the app's setting, else http://localhost:13305).
    #[arg(long, value_name = "URL")]
    pub lemonade_url: Option<String>,

    /// Lemonade model id, e.g. Whisper-Base (default: the app's setting).
    #[arg(long, value_name = "ID")]
    pub lemonade_model: Option<String>,

    /// Lemonade API key. Prefer $HANDY_LEMONADE_API_KEY: arguments are visible
    /// to other processes.
    #[arg(long, value_name = "KEY")]
    pub lemonade_key: Option<String>,

    /// Label speakers ("who said what") with the local speaker model.
    #[arg(long, requires = "transcribe_file")]
    pub speakers: bool,

    /// Sortformer GGUF for --speakers (default: the one Handy downloaded).
    #[arg(long, value_name = "PATH", requires = "speakers")]
    pub speaker_model: Option<PathBuf>,

    /// List the transcription models on the Lemonade Server and exit (honors --json).
    #[arg(long)]
    pub list_lemonade_models: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::path::Path;

    fn parse(args: &[&str]) -> Result<CliArgs, clap::Error> {
        CliArgs::try_parse_from(std::iter::once("handy").chain(args.iter().copied()))
    }

    #[test]
    fn command_definition_is_valid() {
        CliArgs::command().debug_assert();
    }

    #[test]
    fn parses_file_transcription_flags() {
        let args = parse(&["-f", "a.mp3", "--format", "srt", "-o", "x.srt"]).unwrap();
        assert_eq!(args.transcribe_file, Some(PathBuf::from("a.mp3")));
        assert_eq!(args.format, Some(TranscriptFormat::Srt));
        assert_eq!(args.output, Some(PathBuf::from("x.srt")));
        assert!(!args.quiet);

        let args = parse(&[
            "--transcribe-file",
            "talk.opus",
            "--output",
            "talk.vtt",
            "--chunk-secs",
            "20",
            "--ffmpeg",
            "/opt/ffmpeg",
            "-q",
            "--json",
        ])
        .unwrap();
        assert_eq!(args.chunk_secs, Some(20));
        assert_eq!(args.ffmpeg, Some(PathBuf::from("/opt/ffmpeg")));
        assert!(args.quiet && args.json);
        assert_eq!(args.format, None);
    }

    #[test]
    fn format_names_and_aliases() {
        for (name, format) in [
            ("txt", TranscriptFormat::Txt),
            ("md", TranscriptFormat::Md),
            ("markdown", TranscriptFormat::Md),
            ("srt", TranscriptFormat::Srt),
            ("vtt", TranscriptFormat::Vtt),
            ("json", TranscriptFormat::Json),
            ("docx", TranscriptFormat::Docx),
        ] {
            let args = parse(&["-f", "a.wav", "--format", name]).unwrap();
            assert_eq!(args.format, Some(format), "{name}");
        }
        assert!(parse(&["-f", "a.wav", "--format", "pdf"]).is_err());
    }

    #[test]
    fn file_options_require_a_file() {
        assert!(parse(&["-o", "x.txt"]).is_err());
        assert!(parse(&["--format", "srt"]).is_err());
        assert!(parse(&["--chunk-secs", "10"]).is_err());
        assert!(parse(&["--ffmpeg", "/usr/bin/ffmpeg"]).is_err());
        assert!(parse(&["--chunk-secs", "ten", "-f", "a.wav"]).is_err());
        // --quiet stands alone, like --json.
        assert!(parse(&["-q"]).is_ok());
        assert!(parse(&[]).is_ok());
    }

    #[test]
    fn parses_engine_and_lemonade_flags() {
        use crate::settings::FileTranscriptionEngine;
        let args = parse(&[
            "-f",
            "a.mp3",
            "--engine",
            "lemonade",
            "--lemonade-url",
            "http://127.0.0.1:8000/api/v1",
            "--lemonade-model",
            "Whisper-Base",
            "--lemonade-key",
            "k1",
            "--speakers",
            "--speaker-model",
            "/models/sortformer.gguf",
        ])
        .unwrap();
        assert_eq!(args.engine, Some(FileTranscriptionEngine::Lemonade));
        assert_eq!(
            args.lemonade_url.as_deref(),
            Some("http://127.0.0.1:8000/api/v1")
        );
        assert_eq!(args.lemonade_model.as_deref(), Some("Whisper-Base"));
        assert_eq!(args.lemonade_key.as_deref(), Some("k1"));
        assert!(args.speakers);
        assert_eq!(
            args.speaker_model,
            Some(PathBuf::from("/models/sortformer.gguf"))
        );

        let args = parse(&["-f", "a.mp3", "--engine", "handy"]).unwrap();
        assert_eq!(args.engine, Some(FileTranscriptionEngine::Handy));
        assert!(!args.speakers);
        assert!(parse(&["-f", "a.mp3", "--engine", "whisper"]).is_err());
    }

    #[test]
    fn speaker_model_requires_speakers() {
        assert!(parse(&["-f", "a.mp3", "--speaker-model", "s.gguf"]).is_err());
        assert!(parse(&["-f", "a.mp3", "--speakers", "--speaker-model", "s.gguf"]).is_ok());
        assert!(parse(&["--speakers"]).is_err());
    }

    #[test]
    fn engine_requires_a_file() {
        assert!(parse(&["--engine", "lemonade"]).is_err());
        assert!(parse(&["--engine", "lemonade", "-f", "a.wav"]).is_ok());
        // Naming the engine while listing its models is redundant but fine.
        assert!(parse(&["--engine", "lemonade", "--list-lemonade-models"]).is_ok());
    }

    #[test]
    fn list_lemonade_models_stands_alone() {
        let args = parse(&["--list-lemonade-models", "--json"]).unwrap();
        assert!(args.list_lemonade_models && args.json);
        assert!(args.transcribe_file.is_none());
        let args = parse(&[
            "--list-lemonade-models",
            "--lemonade-url",
            "http://localhost:13305",
            "--lemonade-key",
            "k1",
        ])
        .unwrap();
        assert_eq!(args.lemonade_url.as_deref(), Some("http://localhost:13305"));
    }

    #[test]
    fn transcript_format_from_output_extension() {
        let cases = [
            ("notes.txt", Some(TranscriptFormat::Txt)),
            ("notes.TEXT", Some(TranscriptFormat::Txt)),
            ("notes.md", Some(TranscriptFormat::Md)),
            ("notes.Markdown", Some(TranscriptFormat::Md)),
            ("subs.srt", Some(TranscriptFormat::Srt)),
            ("subs.VTT", Some(TranscriptFormat::Vtt)),
            ("subs.webvtt", Some(TranscriptFormat::Vtt)),
            ("out.json", Some(TranscriptFormat::Json)),
            ("out.docx", Some(TranscriptFormat::Docx)),
            ("out.DOCX", Some(TranscriptFormat::Docx)),
            ("out.doc", None),
            ("out", None),
            ("dir.srt/out", None),
        ];
        for (path, format) in cases {
            assert_eq!(
                TranscriptFormat::from_path(Path::new(path)),
                format,
                "{path}"
            );
        }
    }
}
