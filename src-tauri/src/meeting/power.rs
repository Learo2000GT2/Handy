//! Keeping the computer awake during a meeting, and checking free disk
//! space. Neither can stop a closed lid or a pressed power button; the
//! recorder detects those gaps itself.

use std::path::Path;

/// Keeps the system from sleeping while it lives. On Windows the request
/// belongs to the creating thread, so create and drop it on the same
/// long-lived thread.
pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    child: Option<std::process::Child>,
}

impl KeepAwake {
    pub fn start() -> Self {
        #[cfg(windows)]
        {
            use windows::Win32::System::Power::{
                SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
            };
            // SAFETY: plain Win32 call with constant flags.
            let previous = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            if previous.0 == 0 {
                log::warn!("Could not ask Windows to stay awake during the meeting");
            }
            Self {}
        }
        #[cfg(target_os = "macos")]
        {
            let child = std::process::Command::new("caffeinate")
                .args(["-i", "-w", &std::process::id().to_string()])
                .spawn()
                .map_err(|error| log::warn!("Could not start caffeinate: {error}"))
                .ok();
            Self { child }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Self {}
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
            // SAFETY: plain Win32 call; clears this thread's request.
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Bytes free for this user on the disk holding `path`, if known.
pub fn free_space(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut free = 0u64;
        let path = HSTRING::from(path.as_os_str());
        // SAFETY: `free` outlives the call; the other outputs are not requested.
        unsafe { GetDiskFreeSpaceExW(&path, Some(&mut free), None, None) }.ok()?;
        Some(free)
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a valid C string and `stats` a valid out pointer.
        if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(stats.f_bavail as u64 * stats.f_frsize as u64)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_of_a_temp_dir_is_known() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).is_some_and(|bytes| bytes > 0));
    }

    #[test]
    fn keep_awake_starts_and_stops() {
        drop(KeepAwake::start());
    }
}
