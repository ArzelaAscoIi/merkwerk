//! Call detection on Windows and Linux: a call is "another process is using the microphone".
//! Windows: the privacy registry marks an app's mic use as running while LastUsedTimeStop is 0.
//! Linux: PulseAudio / PipeWire (pipewire-pulse) lists the recording streams with their process.

/// Names of other processes currently capturing from the microphone, minus ignored ones
/// (case-insensitive substring match).
pub fn mic_users(ignore: &[String]) -> Vec<String> {
    let mut names = users();
    names.retain(|n| {
        let lower = n.to_lowercase();
        !ignore.iter().any(|i| lower.contains(&i.to_lowercase()))
    });
    names.dedup();
    names
}

/// The call app's process, for mute detection; only on macOS so far.
pub fn call_app_pid(_mic_user: &str) -> Option<i32> {
    None
}

#[cfg(windows)]
fn users() -> Vec<String> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    const MIC: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(MIC) else {
        return vec![];
    };
    // Desktop apps sit under NonPackaged as their path with `#` for `\`.
    let me = std::env::current_exe()
        .map(|p| p.to_string_lossy().replace('\\', "#").to_lowercase())
        .unwrap_or_default();
    let running = |key: &RegKey| {
        key.get_value::<u64, _>("LastUsedTimeStop").is_ok_and(|t| t == 0)
            && key.get_value::<u64, _>("LastUsedTimeStart").is_ok_and(|t| t != 0)
    };
    let mut out = vec![];
    for name in root.enum_keys().flatten() {
        let Ok(key) = root.open_subkey(&name) else { continue };
        if name == "NonPackaged" {
            for path in key.enum_keys().flatten() {
                if path.to_lowercase() == me || !key.open_subkey(&path).is_ok_and(|k| running(&k)) {
                    continue;
                }
                let file = path.rsplit('#').next().unwrap_or(&path);
                out.push(file.trim_end_matches(".exe").trim_end_matches(".EXE").to_string());
            }
        } else if running(&key) {
            // Store apps: `Microsoft.SkypeApp_kzf8qxf38zg5c` -> `SkypeApp`.
            let base = name.split('_').next().unwrap_or(&name);
            out.push(base.rsplit('.').next().unwrap_or(base).to_string());
        }
    }
    out
}

#[cfg(not(windows))]
fn users() -> Vec<String> {
    let Ok(out) = std::process::Command::new("pactl")
        .args(["list", "source-outputs"])
        .env("LC_ALL", "C")
        .output()
    else {
        return vec![];
    };
    parse_pactl(&String::from_utf8_lossy(&out.stdout), std::process::id())
}

/// Streams from `pactl list source-outputs`: running ones, not ours, not a monitor recorder.
#[cfg(not(windows))]
fn parse_pactl(text: &str, me: u32) -> Vec<String> {
    text.split("Source Output #")
        .skip(1)
        .filter_map(|block| {
            let prop = |k: &str| {
                block.lines().find_map(|l| {
                    let v = l.trim().strip_prefix(k)?.trim_start().strip_prefix('=')?;
                    Some(v.trim().trim_matches('"').to_string())
                })
            };
            let corked = block.lines().any(|l| l.trim() == "Corked: yes");
            let pid = prop("application.process.id").and_then(|p| p.parse::<u32>().ok());
            // Our own system-audio recorder (parec) also shows up here.
            let binary = prop("application.process.binary").unwrap_or_default();
            if corked || pid == Some(me) || binary == "parec" {
                return None;
            }
            prop("application.name").or(Some(binary)).filter(|n| !n.is_empty())
        })
        .collect()
}

#[cfg(all(test, not(windows)))]
mod tests {
    #[test]
    fn pactl_streams() {
        let text = "Source Output #12\n\tCorked: no\n\tProperties:\n\t\tapplication.name = \"Firefox\"\n\t\tapplication.process.id = \"77\"\n\
Source Output #13\n\tCorked: yes\n\tProperties:\n\t\tapplication.name = \"Zoom\"\n\
Source Output #14\n\tCorked: no\n\tProperties:\n\t\tapplication.process.binary = \"parec\"\n\
Source Output #15\n\tCorked: no\n\tProperties:\n\t\tapplication.name = \"merkwerk\"\n\t\tapplication.process.id = \"5\"\n";
        assert_eq!(super::parse_pactl(text, 5), ["Firefox"]);
    }
}
