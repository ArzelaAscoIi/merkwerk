//! Screen activity on Windows and Linux: the frontmost window's app and title.
// ponytail: titles only. Window content needs UI Automation (Windows) or AT-SPI (Linux); add when
// titles turn out too thin. On Wayland only XWayland windows are visible.

pub struct Snapshot {
    pub app: String,
    pub bundle: Option<String>,
    pub window: String,
    pub url: Option<String>,
    pub text: String,
}

/// Apps and windows that are never captured (name or window title fragments).
pub const DEFAULT_IGNORE: &str = "merkwerk,1password,bitwarden,lastpass,dashlane,keepass,\
private browsing,incognito,inprivate";

/// No permission to ask for outside macOS.
pub fn trusted(_prompt: bool) -> bool {
    true
}

pub fn frontmost_app() -> Option<String> {
    front().map(|(app, _)| app)
}

pub fn snapshot(_prepared: &mut std::collections::HashSet<i32>) -> Option<Snapshot> {
    let (app, window) = front()?;
    Some(Snapshot {
        app,
        bundle: None,
        window,
        url: None,
        text: String::new(),
    })
}

/// Case-insensitive fragment match against app name and window title.
pub fn ignored(s: &Snapshot, ignore: &[String]) -> bool {
    let hay = format!("{} {}", s.app, s.window).to_lowercase();
    ignore.iter().any(|i| hay.contains(i.as_str()))
}

#[cfg(windows)]
pub fn idle_secs() -> f64 {
    use windows_sys::Win32::{System::SystemInformation::GetTickCount, UI::Input::KeyboardAndMouse::*};
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return 0.0;
    }
    unsafe { GetTickCount() }.wrapping_sub(info.dwTime) as f64 / 1000.0
}

/// (app, window title) of the foreground window.
#[cfg(windows)]
fn front() -> Option<(String, String)> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW},
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId},
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == std::process::id() {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let app = std::path::Path::new(&path).file_stem()?.to_string_lossy().into_owned();
        Some((app, title))
    }
}

/// X11 idle time through `xprintidle` if installed; otherwise "active", so nothing is skipped.
#[cfg(not(windows))]
pub fn idle_secs() -> f64 {
    run("xprintidle", &[])
        .and_then(|ms| ms.trim().parse::<f64>().ok())
        .map_or(0.0, |ms| ms / 1000.0)
}

/// (app, window title) of the active X11 window, through `xprop`.
#[cfg(not(windows))]
fn front() -> Option<(String, String)> {
    let active = run("xprop", &["-root", "_NET_ACTIVE_WINDOW"])?;
    let id = active.split_whitespace().last()?.to_string();
    let props = run("xprop", &["-id", &id, "_NET_WM_PID", "_NET_WM_NAME"])?;
    let value = |key: &str| {
        props
            .lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_once(" = "))
            .map(|(_, v)| v.trim().to_string())
    };
    let pid: u32 = value("_NET_WM_PID")?.parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    let app = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()?
        .trim()
        .to_string();
    let title = value("_NET_WM_NAME").unwrap_or_default().trim_matches('"').to_string();
    Some((app, title))
}

#[cfg(not(windows))]
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
