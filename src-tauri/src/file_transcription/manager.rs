//! The GUI's file transcription queue: one worker thread, job state, events
//! and history saving (WP4).
#![allow(dead_code, unused_variables, unused_imports)]

use crate::file_transcription::types::{FileJobSnapshot, FileTranscript, FileTranscriptionOptions};
use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Condvar, Mutex};
use tauri::AppHandle;

/// WP4: private, pure job table (no AppHandle) so it can be unit-tested.
struct JobTable {}

pub struct FileTranscriptionManager {
    app: AppHandle,
    tm: Arc<TranscriptionManager>,
    history: Arc<HistoryManager>,
    state: Mutex<JobTable>,
    wake: Condvar,
    next_id: AtomicU64,
}

impl FileTranscriptionManager {
    /// Spawns the worker thread "file-transcription".
    pub fn new(
        app: &AppHandle,
        tm: Arc<TranscriptionManager>,
        history: Arc<HistoryManager>,
    ) -> Arc<Self> {
        todo!("WP4")
    }

    /// Validates each path (exists + is_file → else Failed NotFound; route_for_path → else Failed
    /// UnsupportedFormat). Valid ones are Queued. Emits Updated per job. Never blocks on work.
    pub fn enqueue(
        &self,
        paths: Vec<String>,
        options: FileTranscriptionOptions,
    ) -> Vec<FileJobSnapshot> {
        todo!("WP4")
    }

    /// Queued → Cancelled at once. Running → token cancel + tm.cancel_file_work(&token);
    /// the status changes when the pipeline returns. Finished/unknown → Err.
    pub fn cancel(&self, job_id: u64) -> Result<(), String> {
        todo!("WP4")
    }

    pub fn cancel_all(&self) {
        todo!("WP4")
    }

    /// Enqueue order.
    pub fn list(&self) -> Vec<FileJobSnapshot> {
        todo!("WP4")
    }

    /// Only finished jobs; frees the result; emits Removed.
    pub fn dismiss(&self, job_id: u64) -> Result<(), String> {
        todo!("WP4")
    }

    /// Finished: stored result. Running: what is done so far (status Partial,
    /// status_detail "Transcription in progress"). None if unknown or nothing yet.
    pub fn transcript(&self, job_id: u64) -> Option<FileTranscript> {
        todo!("WP4")
    }
}
