//! Push-to-talk dictation: hold the dictation key, speak, let go; the text lands in the focused app.
//! Same local speech model as meetings, mic only. Each dictation's text is kept in
//! `$MERKWERK_DIR/dictations.jsonl` for the history in the notes window; no audio is saved.

use crate::platform::keys::{self as sys, MODIFIERS, down, named, presses};
use crate::state::AppState;
use crate::{
    audio::{self, Source},
    config::Config,
    transcribe::Transcriber,
};
use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

/// Recording a new shortcut from the notes window.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Capture {
    #[default]
    Off,
    /// Waiting for the user to press and release their keys.
    Listening,
    Failed(String),
}

/// "⌃ ⌥", "Right ⌘"; empty for no shortcut.
pub fn label(keys: &[u16]) -> String {
    MODIFIERS
        .iter()
        .filter(|(code, _)| keys.contains(code))
        .map(|(_, l)| *l)
        .collect::<Vec<_>>()
        .join(" ")
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
pub fn set_shortcut(cfg: &Config, state: &AppState, keys: Vec<u16>) {
    let _ = crate::config::set_setting(&cfg.root, "dictate_key", serde_json::json!(keys));
    state.shortcut.set(keys);
}

/// Waits for the user to press and release a combination of modifier keys and saves it.
fn capture(cfg: &Config, state: &AppState) {
    if let Some(why) = sys::UNAVAILABLE {
        *state.capture.lock() = Capture::Failed(why.into());
        return;
    }
    let before = presses();
    let start = Instant::now();
    let mut seen: Vec<u16> = vec![];
    while *state.capture.lock() == Capture::Listening {
        thread::sleep(Duration::from_millis(10));
        if presses() != before {
            *state.capture.lock() = Capture::Failed(sys::ONLY_MODIFIERS.into());
            return;
        }
        let held: Vec<u16> = MODIFIERS.iter().map(|m| m.0).filter(|k| down(*k)).collect();
        for k in &held {
            if !seen.contains(k) {
                seen.push(*k);
            }
        }
        if !seen.is_empty() && held.is_empty() {
            set_shortcut(cfg, state, seen);
            *state.capture.lock() = Capture::Off;
            return;
        }
        if seen.is_empty() && start.elapsed() > Duration::from_secs(15) {
            *state.capture.lock() = Capture::Off;
            return;
        }
    }
}

/// What the dictation overlay shows; the UI polls it.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Overlay {
    #[default]
    Hidden,
    Listening,
    Transcribing,
    /// The dictated text or a problem, shown until the instant passes.
    Said(String, Instant),
}

fn show(state: &AppState, o: Overlay) {
    state.overlay.set(o);
}

/// Show `text` in the overlay for a few seconds.
fn say(state: &AppState, text: impl Into<String>) {
    show(
        state,
        Overlay::Said(text.into(), Instant::now() + Duration::from_secs(4)),
    );
}

/// The speech model stays loaded this long after the last dictation, then is released.
const KEEP_MODEL: Duration = Duration::from_secs(600);

/// Holds shorter than this are taps or shortcuts; the mic isn't even opened.
const HOLD: Duration = Duration::from_millis(250);

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
pub fn run(cfg: Config, state: Arc<AppState>) {
    let mut model: Option<(String, Transcriber)> = None;
    let mut last_used = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(20));
        if model.is_some() && last_used.elapsed() > KEEP_MODEL {
            model = None;
        }
        if *state.capture.lock() == Capture::Listening {
            capture(&cfg, &state);
            continue;
        }
        let keys = state.shortcut.get();
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
        let (stream, mut resampler) = match audio::mic(tx, &state.audio_level) {
            Ok(mic) => mic,
            Err(e) => {
                eprintln!("dictation mic: {e:#}");
                say(&state, format!("No microphone: {e}"));
                wait(|| held(&keys));
                continue;
            }
        };
        show(&state, Overlay::Listening);
        wait(|| held(&keys) && presses() == before);
        drop(stream);
        if presses() != before {
            show(&state, Overlay::Hidden);
            wait(|| held(&keys));
            continue;
        }
        show(&state, Overlay::Transcribing);
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
                    say(&state, format!("Speech model missing: {e}"));
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
                    say(&state, "Didn't catch that");
                    continue;
                }
                remember(&cfg, &text);
                say(
                    &state,
                    if sys::paste(&text) {
                        text
                    } else {
                        format!("Allow Accessibility to paste. Copy it from the notes window: {text}")
                    },
                );
            }
            Err(e) => {
                eprintln!("dictation: {e:#}");
                say(&state, format!("Transcription failed: {e}"));
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
pub fn page_state(cfg: &Config, state: &AppState) -> String {
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
    let capture = state.capture.get();
    serde_json::json!({
        "shortcut": label(&state.shortcut.lock()),
        "capturing": capture == Capture::Listening,
        "error": match capture { Capture::Failed(e) => Some(e), _ => None },
        "trusted": crate::platform::screen::trusted(false),
        "unavailable": sys::UNAVAILABLE,
        "chosen": crate::config::setting(&cfg.root, "dictate_key").is_some(),
        "history": history,
    })
    .to_string()
    .replace('<', "\\u003c")
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(target_os = "macos")]
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
