//! Push-to-talk dictation: hold the dictation key, speak, let go; the text lands in the focused app.
//! Same local speech model as meetings, mic only. Each dictation's text is kept in
//! `$MERKWERK_DIR/dictations.jsonl` for the history in the notes window; no audio is saved.

use crate::{
    audio::{self, Source},
    config::Config,
    transcribe::Transcriber,
};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use std::{
    ffi::c_void,
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

/// Keys a dictation shortcut may use, in display order: macOS virtual key code and label.
/// Modifiers only: holding them types nothing, so any other key press or click during the hold
/// means a shortcut (⌘C, ⌥-letter, ⌘-click) and cancels the dictation.
// ponytail: a letter or Space in the shortcut would also be typed into the app; allowing those
// needs a CGEventTap that swallows them.
const MODIFIERS: [(u16, &str); 9] = [
    (63, "fn"),
    (59, "⌃"),
    (62, "Right ⌃"),
    (58, "⌥"),
    (61, "Right ⌥"),
    (56, "⇧"),
    (60, "Right ⇧"),
    (55, "⌘"),
    (54, "Right ⌘"),
];

/// The dictation shortcut: these keys held together. Empty = off.
pub static SHORTCUT: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// Recording a new shortcut from the notes window.
#[derive(Clone, PartialEq)]
pub enum Capture {
    Off,
    /// Waiting for the user to press and release their keys.
    Listening,
    Failed(String),
}

pub static CAPTURE: Mutex<Capture> = Mutex::new(Capture::Off);

/// "⌃ ⌥", "Right ⌘"; empty for no shortcut.
pub fn label(keys: &[u16]) -> String {
    MODIFIERS
        .iter()
        .filter(|(code, _)| keys.contains(code))
        .map(|(_, l)| *l)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Names for `MERKWERK_DICTATE` and settings from before shortcuts were recorded, e.g. "right_command"
/// or "control+option".
fn named(name: &str) -> Option<u16> {
    Some(match name.trim() {
        "fn" => 63,
        "control" | "ctrl" => 59,
        "right_control" => 62,
        "option" | "alt" => 58,
        "right_option" => 61,
        "shift" => 56,
        "right_shift" => 60,
        "command" | "cmd" => 55,
        "right_command" => 54,
        _ => return None,
    })
}

/// The shortcut from `MERKWERK_DICTATE`, else `dictate_key` in settings.json (a list of key codes, or
/// an old single name), else none.
pub fn configured(cfg: &Config) -> Vec<u16> {
    let names = |s: &str| s.split('+').filter_map(named).collect::<Vec<_>>();
    if let Ok(v) = std::env::var("MERKWERK_DICTATE") {
        return names(&v);
    }
    match crate::config::setting(&cfg.root, "dictate_key") {
        Some(serde_json::Value::Array(codes)) => codes
            .iter()
            .filter_map(|c| c.as_u64())
            .map(|c| c as u16)
            .filter(|c| MODIFIERS.iter().any(|m| m.0 == *c))
            .collect(),
        Some(serde_json::Value::String(s)) => names(&s),
        _ => vec![],
    }
}

/// Sets and saves the shortcut (empty turns dictation off).
pub fn set_shortcut(cfg: &Config, keys: Vec<u16>) {
    let _ = crate::config::set_setting(&cfg.root, "dictate_key", serde_json::json!(keys));
    *SHORTCUT.lock().unwrap() = keys;
}

/// Waits for the user to press and release a combination of modifier keys and saves it.
fn capture(cfg: &Config) {
    let before = presses();
    let start = Instant::now();
    let mut seen: Vec<u16> = vec![];
    while *CAPTURE.lock().unwrap() == Capture::Listening {
        thread::sleep(Duration::from_millis(10));
        if presses() != before {
            *CAPTURE.lock().unwrap() =
                Capture::Failed("Use only fn, ⌃, ⌥, ⇧ and ⌘: any other key would also be typed into the app.".into());
            return;
        }
        let held: Vec<u16> = MODIFIERS.iter().map(|m| m.0).filter(|k| down(*k)).collect();
        for k in &held {
            if !seen.contains(k) {
                seen.push(*k);
            }
        }
        if !seen.is_empty() && held.is_empty() {
            set_shortcut(cfg, seen);
            *CAPTURE.lock().unwrap() = Capture::Off;
            return;
        }
        if seen.is_empty() && start.elapsed() > Duration::from_secs(15) {
            *CAPTURE.lock().unwrap() = Capture::Off;
            return;
        }
    }
}

/// What the dictation overlay shows; the UI polls it.
#[derive(Clone, PartialEq)]
pub enum Overlay {
    Hidden,
    Listening,
    Transcribing,
    /// The dictated text or a problem, shown until the instant passes.
    Said(String, Instant),
}

pub static OVERLAY: Mutex<Overlay> = Mutex::new(Overlay::Hidden);

fn show(o: Overlay) {
    *OVERLAY.lock().unwrap() = o;
}

/// Show `text` in the overlay for a few seconds.
fn say(text: impl Into<String>) {
    show(Overlay::Said(text.into(), Instant::now() + Duration::from_secs(4)));
}

/// The speech model stays loaded this long after the last dictation, then is released.
const KEEP_MODEL: Duration = Duration::from_secs(600);

/// Holds shorter than this are taps or shortcuts; the mic isn't even opened.
const HOLD: Duration = Duration::from_millis(250);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGEventSourceCounterForEventType(state: i32, event_type: u32) -> u32;
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> *mut c_void;
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventPost(tap: u32, event: *mut c_void);
    fn CFRelease(cf: *const c_void);
}

/// HID system state: the physical keys, not what apps posted.
const HID: i32 = 1;

fn down(key: u16) -> bool {
    unsafe { CGEventSourceKeyState(HID, key) }
}

/// Key downs and mouse clicks so far; a change during a hold means it was a shortcut or ⌘-click.
fn presses() -> u32 {
    unsafe { CGEventSourceCounterForEventType(HID, 10) + CGEventSourceCounterForEventType(HID, 1) }
}

/// The whole shortcut is held.
fn held(keys: &[u16]) -> bool {
    !keys.is_empty() && keys.iter().all(|k| down(*k))
}

fn wait(cond: impl Fn() -> bool) {
    while cond() {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Polls the dictation shortcut; runs forever on its own thread.
// ponytail: 20 ms polling instead of an event tap; no run loop, no Input Monitoring grant.
pub fn run(cfg: Config) {
    let mut model: Option<(String, Transcriber)> = None;
    let mut last_used = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(20));
        if model.is_some() && last_used.elapsed() > KEEP_MODEL {
            model = None;
        }
        if *CAPTURE.lock().unwrap() == Capture::Listening {
            capture(&cfg);
            continue;
        }
        let keys = SHORTCUT.lock().unwrap().clone();
        if !held(&keys) {
            continue;
        }
        let before = presses();
        let start = Instant::now();
        wait(|| held(&keys) && start.elapsed() < HOLD);
        if !held(&keys) || presses() != before {
            wait(|| held(&keys));
            continue;
        }
        let (tx, rx) = mpsc::channel();
        let (stream, mut resampler) = match audio::mic(tx) {
            Ok(mic) => mic,
            Err(e) => {
                eprintln!("dictation mic: {e:#}");
                say(format!("No microphone: {e}"));
                wait(|| held(&keys));
                continue;
            }
        };
        show(Overlay::Listening);
        wait(|| held(&keys) && presses() == before);
        drop(stream);
        if presses() != before {
            show(Overlay::Hidden);
            wait(|| held(&keys));
            continue;
        }
        show(Overlay::Transcribing);
        let mut samples = vec![];
        for (_, block) in rx.try_iter() {
            resampler.process(&block, &mut samples);
        }
        // Load once, again only when the menu switches engines.
        let engine = cfg.engine();
        if model.as_ref().is_none_or(|(e, _)| *e != engine) {
            model = match Transcriber::load(&cfg) {
                Ok(t) => Some((engine, t)),
                Err(e) => {
                    eprintln!("dictation model: {e:#}");
                    say(format!("Speech model missing: {e}"));
                    continue;
                }
            };
        }
        let Some((_, t)) = &mut model else { continue };
        last_used = Instant::now();
        match t.run(Source::Me, 0.0, &samples) {
            Ok(segments) => {
                let text = join(segments.iter().map(|s| s.text.as_str()));
                if text.is_empty() {
                    say("Didn't catch that");
                    continue;
                }
                remember(&cfg, &text);
                say(if paste(&text) {
                    text
                } else {
                    format!("Allow Accessibility to paste. Copy it from the notes window: {text}")
                });
            }
            Err(e) => {
                eprintln!("dictation: {e:#}");
                say(format!("Transcription failed: {e}"));
            }
        }
    }
}

fn join<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn history_file(cfg: &Config) -> std::path::PathBuf {
    cfg.root.join("dictations.jsonl")
}

fn remember(cfg: &Config, text: &str) {
    use std::io::Write;
    let line = serde_json::json!({"at": chrono::Local::now().to_rfc3339(), "text": text});
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_file(cfg));
    if let Err(e) = file.and_then(|mut f| writeln!(f, "{line}")) {
        eprintln!("dictation history: {e}");
    }
}

/// What the notes window's Dictation tab shows: chosen key, the options, whether pasting is
/// allowed, whether a key was ever chosen, and the latest dictations (newest first).
// ponytail: rereads the history file on every call (once a second while the window is open);
// cache by mtime if it ever gets big. Only the last 200 are sent.
pub fn page_state(cfg: &Config) -> String {
    let history: Vec<serde_json::Value> = std::fs::read_to_string(history_file(cfg))
        .unwrap_or_default()
        .lines()
        .rev()
        .take(200)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| {
            let at = chrono::DateTime::parse_from_rfc3339(v["at"].as_str().unwrap_or_default()).ok();
            serde_json::json!({
                "day": at.map(|a| a.format("%A, %b %-d").to_string()),
                "time": at.map(|a| a.format("%H:%M").to_string()),
                "text": v["text"],
            })
        })
        .collect();
    let capture = CAPTURE.lock().unwrap().clone();
    serde_json::json!({
        "shortcut": label(&SHORTCUT.lock().unwrap()),
        "capturing": capture == Capture::Listening,
        "error": match capture { Capture::Failed(e) => Some(e), _ => None },
        "trusted": crate::screen::trusted(false),
        "chosen": crate::config::setting(&cfg.root, "dictate_key").is_some(),
        "history": history,
    })
    .to_string()
    .replace('<', "\\u003c")
}

/// Pastes through the clipboard (⌘V), then puts the previous text back. False without Accessibility.
// ponytail: restores plain text only; an image on the clipboard is lost. Key code 9 is V on
// QWERTY/QWERTZ; Dvorak would need the layout lookup.
fn paste(text: &str) -> bool {
    if !crate::screen::trusted(false) {
        eprintln!("dictation: allow Accessibility for merkwerk to paste");
        return false;
    }
    let pb = NSPasteboard::generalPasteboard();
    let kind = unsafe { NSPasteboardTypeString };
    let old = pb.stringForType(kind);
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(text), kind);
    unsafe {
        for pressed in [true, false] {
            let e = CGEventCreateKeyboardEvent(std::ptr::null(), 9, pressed);
            CGEventSetFlags(e, 1 << 20); // command
            CGEventPost(0, e);
            CFRelease(e);
        }
    }
    // The target app reads the clipboard asynchronously after ⌘V.
    thread::sleep(Duration::from_millis(300));
    if let Some(old) = old {
        pb.clearContents();
        pb.setString_forType(&old, kind);
    }
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn shortcut_names_and_join() {
        assert_eq!(super::label(&[58, 59]), "⌃ ⌥", "display order, not press order");
        assert_eq!(super::label(&[54]), "Right ⌘");
        let names = |s: &str| s.split('+').filter_map(super::named).collect::<Vec<_>>();
        assert_eq!(names("control+option"), [59, 58]);
        assert_eq!(names("right_command"), [54]);
        assert!(names("off").is_empty());
        assert_eq!(super::join([" Hallo ", "", "world."].into_iter()), "Hallo world.");
    }
}
