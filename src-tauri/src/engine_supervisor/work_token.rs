//! A cancel handle for one caller's engine work (a file transcription job).
//! Cancelling it never affects work submitted without it (dictation).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct WorkToken(Arc<AtomicBool>);

impl WorkToken {
    pub fn new() -> Self {
        Self::default()
    }
    /// Mark cancelled. Does not stop an engine call already in flight; use
    /// `EngineSupervisor::cancel_work` / `TranscriptionManager::cancel_file_work`.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    /// The raw flag, for code that polls an `&AtomicBool` (decoders).
    pub fn flag(&self) -> &AtomicBool {
        &self.0
    }
    pub fn same_as(&self, other: &WorkToken) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
