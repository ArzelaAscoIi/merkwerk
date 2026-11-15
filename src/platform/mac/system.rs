//! Small OS services: the account name, opening files, the Trash, memory, the log file.

use anyhow::{Context, Result};
use std::{io::IsTerminal, os::fd::AsRawFd, path::Path, process::Command};

/// First name from the macOS account, e.g. "Kristof".
pub fn account_first_name() -> Option<String> {
    let out = Command::new("id").arg("-F").output().ok()?;
    let name = String::from_utf8(out.stdout).ok()?;
    name.split_whitespace().next().map(str::to_string)
}

/// Opens a folder, file or URL with the system's default handler.
pub fn open(target: &Path) {
    let _ = Command::new("open").arg(target).spawn();
}

/// Moves a directory to the Trash, so a mistaken delete can be undone.
pub fn trash(path: &Path) -> Result<()> {
    let url = objc2_foundation::NSURL::from_directory_path(path).context("path is not a directory")?;
    objc2_foundation::NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))
}

/// Hands freed pages back to the OS instead of letting the allocator keep them for the rest of the day.
pub fn release_memory() {
    unsafe extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: a null zone means all zones; the call only returns memory the allocator no longer uses.
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
}

/// Started as merkwerk.app, stderr goes nowhere; keep it in `<dir>/merkwerk.log` instead.
pub fn log_to_file(dir: &Path) {
    if std::io::stderr().is_terminal() {
        return;
    }
    unsafe extern "C" {
        fn dup2(from: i32, to: i32) -> i32;
    }
    let _ = std::fs::create_dir_all(dir);
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("merkwerk.log"))
    {
        // SAFETY: both descriptors are valid; fd 2 is stderr, replaced by the open log file.
        unsafe { dup2(file.as_raw_fd(), 2) };
    }
}
