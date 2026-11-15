//! Small OS services: the account name, opening files, the Trash, memory, the log file.

use anyhow::Result;
use std::{env, path::Path, process::Command};

/// The login name.
pub fn account_first_name() -> Option<String> {
    env::var("USER").or_else(|_| env::var("USERNAME")).ok()
}

/// Opens a folder, file or URL with the system's default handler.
pub fn open(target: &Path) {
    #[cfg(windows)]
    let mut cmd = Command::new("explorer");
    #[cfg(target_os = "linux")]
    let mut cmd = Command::new("xdg-open");
    let _ = cmd.arg(target).spawn();
}

/// Moves a directory to the Recycle Bin / Trash, so a mistaken delete can be undone.
pub fn trash(path: &Path) -> Result<()> {
    trash::delete(path).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Hands freed pages back to the OS instead of letting the allocator keep them for the rest of the day.
pub fn release_memory() {
    #[cfg(target_os = "linux")]
    // SAFETY: `malloc_trim` only returns free heap pages to the OS.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Linux: started from a desktop entry, stderr goes nowhere; keep it in `<dir>/merkwerk.log` instead.
/// Windows keeps its console window for the log.
// ponytail: a GUI subsystem build + log file on Windows when the console bothers anyone.
pub fn log_to_file(dir: &Path) {
    #[cfg(target_os = "linux")]
    {
        use std::{io::IsTerminal, os::fd::AsRawFd};
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
    #[cfg(windows)]
    let _ = dir;
}
