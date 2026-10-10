//! Transcription of existing audio/video files:
//! decode → 16 kHz mono → silence-aware chunks → transcribe → export.
pub mod chunker;
pub mod decode;
pub mod diarize;
pub mod docx;
pub mod engine;
pub mod export;
pub mod ffmpeg;
pub mod formats;
pub mod label_job;
pub mod lemonade;
pub mod manager;
pub mod pipeline;
pub mod resample;
pub mod speaker_model;
pub mod speakers;
pub mod types;

pub use manager::FileTranscriptionManager;
