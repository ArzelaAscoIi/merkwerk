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
    sync::{
        Mutex,
        atomic::{AtomicU16, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Dictation keys offered in the menu: setting value, label, macOS virtual key code.
/// Modifier keys only: holding one sends no key events of its own, so any key press during the
/// hold means a shortcut (⌘C, ⌥-letter) and cancels the dictation.
pub const KEYS: [(&str, &str, u16); 6] = [
    ("off", "Off", 0),
    ("fn", "fn (set \"Press 🌐 key to\" to \"Do Nothing\")", 63),
    ("right_command", "Right ⌘", 54),
    ("right_option", "Right ⌥", 61),
    ("right_control", "Right ⌃", 62),
    ("right_shift", "Right ⇧", 60),
];

/// Key code of the chosen dictation key, 0 = off. Set at start and from the menu.
pub static KEY: AtomicU16 = AtomicU16::new(0);
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

pub fn code(name: &str) -> u16 {
    KEYS.iter().find(|k| k.0 == name).map_or(0, |k| k.2)
}

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

fn wait(cond: impl Fn() -> bool) {
    while cond() {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Polls the dictation key; runs forever on its own thread.
// ponytail: 20 ms polling instead of an event tap; no run loop, no Input Monitoring grant.
pub fn run(cfg: Config) {
    let mut model: Option<(String, Transcriber)> = None;
    let mut last_used = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(20));
        if model.is_some() && last_used.elapsed() > KEEP_MODEL {
            model = None;
        }
        let key = KEY.load(Ordering::Relaxed);
        if key == 0 || !down(key) {
            continue;
        }
        let before = presses();
        let start = Instant::now();
        wait(|| down(key) && start.elapsed() < HOLD);
        if !down(key) || presses() != before {
            wait(|| down(key));
            continue;
        }
        let (tx, rx) = mpsc::channel();
        let (stream, mut resampler) = match audio::mic(tx) {
            Ok(mic) => mic,
            Err(e) => {
                eprintln!("dictation mic: {e:#}");
                say(format!("No microphone: {e}"));
                wait(|| down(key));
                continue;
            }
        };
        show(Overlay::Listening);
        wait(|| down(key) && presses() == before);
        drop(stream);
        if presses() != before {
            show(Overlay::Hidden);
            wait(|| down(key));
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
    let key = KEY.load(Ordering::Relaxed);
    serde_json::json!({
        "key": KEYS.iter().find(|k| k.2 == key).map_or("off", |k| k.0),
        "keys": KEYS.iter().map(|k| [k.0, k.1]).collect::<Vec<_>>(),
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
    fn keys_and_join() {
        assert_eq!(super::code("right_command"), 54);
        assert_eq!(super::code("nonsense"), 0);
        assert_eq!(super::join([" Hallo ", "", "world."].into_iter()), "Hallo world.");
    }
}
