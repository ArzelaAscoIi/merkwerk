//! The status UI, shared by the macOS menu bar app (`mac`) and the tray app on Windows and Linux
//! (`other`): what the menu shows, what its items and the notes page do, and the live state behind
//! the recording pill. The platform modules only draw it.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(not(target_os = "macos"))]
mod other;
#[cfg(target_os = "macos")]
pub use mac::{run, run_notes};
#[cfg(not(target_os = "macos"))]
pub use other::{run, run_notes};

use crate::{config::Config, notes, store::Store};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// Set by the watch loop: the app being recorded and when it started.
pub static RECORDING: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// Ids of meetings still being transcribed / summarized.
pub static FINISHING: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The menu asked for the Dictation tab; switched to on the next refresh, once the page has loaded.
static SHOW_DICTATION: AtomicBool = AtomicBool::new(false);
/// Menu clicks, consumed by the watch loop.
pub static START: AtomicBool = AtomicBool::new(false);
pub static STOP: AtomicBool = AtomicBool::new(false);
/// Ignore the detected call (until its app releases the mic).
pub static IGNORE: AtomicBool = AtomicBool::new(false);
/// Auto-record detected calls; off by default, persisted in settings.json.
pub static AUTO: AtomicBool = AtomicBool::new(false);
/// Record screen activity; on by default, persisted in settings.json.
pub static SCREEN: AtomicBool = AtomicBool::new(false);
/// A call detected in manual mode, waiting for Record or Ignore.
pub static PENDING: Mutex<Option<String>> = Mutex::new(None);
/// Why notes can't be written right now (no server, model missing), in plain words; None when fine
/// or unknown (a hosted endpoint without a model list).
static NOTES_PROBLEM: Mutex<Option<String>> = Mutex::new(None);
/// Models offered by the notes server, refreshed in the background.
static SERVER_MODELS: Mutex<Vec<String>> = Mutex::new(vec![]);
/// Chat answers waiting to be handed to the page: (request id, answer HTML).
static ANSWERS: Mutex<Vec<(u64, String)>> = Mutex::new(vec![]);
/// Speech engine whose model is being downloaded.
static DOWNLOADING: Mutex<Option<String>> = Mutex::new(None);

/// Download the selected engine's missing model files in the background, one download at a time;
/// the menu shows "(downloading...)" meanwhile.
pub fn download_models(cfg: Config) {
    let engine = cfg.engine();
    {
        let mut downloading = DOWNLOADING.lock().unwrap();
        if downloading.is_some() {
            return;
        }
        *downloading = Some(engine);
    }
    std::thread::spawn(move || {
        // Loop: the engine may be switched mid-download; a pick that found DOWNLOADING set relies
        // on this thread. The installed check and clearing share the lock so no pick slips between.
        loop {
            {
                let mut downloading = DOWNLOADING.lock().unwrap();
                let engine = cfg.engine();
                if crate::transcribe::installed(&cfg, &engine) {
                    *downloading = None;
                    return;
                }
                *downloading = Some(engine);
            }
            if let Err(e) = crate::transcribe::ensure_models(&cfg) {
                eprintln!("model download: {e:#}");
                *DOWNLOADING.lock().unwrap() = None;
                return;
            }
        }
    });
}

/// Speech engines offered in the menu: id and label.
const ENGINES: [(&str, &str); 3] = [
    ("parakeet", "Parakeet v3 (25 languages, fastest)"),
    ("whisper", "Whisper large-v3-turbo (99 languages)"),
    (
        "whisper-v2",
        "Whisper large-v2 (99 languages, slower, often more accurate)",
    ),
];

const RECENT: usize = 8;

#[derive(Debug, Clone)]
enum Pick {
    Engine(String),
    Llm(String),
    Server(String),
    Never(String),
}

/// "Never record <app>" (`record: false`) or its undo: moves the app's shown name ("Vivaldi
/// Helper" -> "Vivaldi", covering all its helpers) between settings.json `ignore` and `allow`;
/// `allow` also overrides the built-in defaults. Never also stops a running recording of the app.
fn set_recorded(cfg: &Config, app: String, record: bool) {
    let app = notes::app_name(&app);
    if !record
        && RECORDING
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|r| notes::app_name(&r.0).eq_ignore_ascii_case(&app))
    {
        STOP.store(true, Ordering::Relaxed);
    }
    let (add, remove) = if record {
        ("allow", "ignore")
    } else {
        ("ignore", "allow")
    };
    for key in [remove, add] {
        let mut apps: Vec<String> = crate::config::setting(&cfg.root, key)
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        apps.retain(|a| !a.eq_ignore_ascii_case(&app));
        if key == add {
            apps.push(app.clone());
        }
        if let Err(e) = crate::config::set_setting(&cfg.root, key, apps.into()) {
            eprintln!("{key}: {e}");
        }
    }
}

/// Keeps SERVER_MODELS current (every 10 s) from the notes server's `/v1/models`.
/// Auto-record on or off, saved to settings.json.
fn set_auto(on: bool) {
    AUTO.store(on, Ordering::Relaxed);
    if let Some((cfg, _)) = store() {
        let _ = crate::config::set_setting(&cfg.root, "auto_record", on.into());
    }
}

/// Writes a meeting's notes again in the background (the Write notes button), like after a recording.
fn write_notes(cfg: Config, store: Store, id: String) {
    let mut finishing = FINISHING.lock().unwrap();
    if finishing.contains(&id) || store.load(&id).is_ok_and(|m| store.live(&m)) {
        return;
    }
    finishing.push(id.clone());
    std::thread::spawn(move || {
        if let Err(e) = crate::session::finalize(&cfg, &store, &id) {
            eprintln!("notes for {id}: {e:#}");
        }
        FINISHING.lock().unwrap().retain(|f| *f != id);
    });
}

fn watch_models(cfg: &Config) {
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        loop {
            // Re-read each time: the menu can switch servers.
            let url = cfg.models_url();
            let names: Option<Vec<String>> = url.as_ref().and_then(|url| {
                let mut req = ureq::get(url);
                if let Some(key) = &cfg.llm_key {
                    req = req.header("Authorization", &format!("Bearer {key}"));
                }
                req.call()
                    .ok()
                    .and_then(|mut r| r.body_mut().read_json::<serde_json::Value>().ok())
                    .and_then(|v| {
                        v["data"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
                    })
            });
            let server = crate::summarize::server_name(&cfg);
            let model = cfg.llm_model();
            // Ollama lists `gemma4` as `gemma4:latest`.
            let has = |n: &Vec<String>| {
                n.iter()
                    .any(|m| *m == model || m.strip_suffix(":latest") == Some(model.as_str()))
            };
            *NOTES_PROBLEM.lock().unwrap() = match (&url, &names) {
                (None, _) => None,
                (Some(_), None) => Some(format!(
                    "No notes model is running: {server} isn't reachable on this computer."
                )),
                (Some(_), Some(n)) if n.is_empty() => Some(format!("{server} is running but has no model installed.")),
                (Some(_), Some(n)) if !has(n) => Some(format!(
                    "The notes model “{model}” isn't installed in {server}. Pick another one under Notes model in the menu."
                )),
                _ => None,
            };
            // Empty on failure so a previous server's models don't linger.
            let mut names = names.unwrap_or_default();
            names.sort();
            *SERVER_MODELS.lock().unwrap() = names;
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    });
}

fn title() -> String {
    if let Some((_, since)) = &*RECORDING.lock().unwrap() {
        let s = since.elapsed().as_secs();
        return format!("● {:02}:{:02}", s / 60, s % 60);
    }
    if !FINISHING.lock().unwrap().is_empty() {
        "◌ mw".into()
    } else {
        "mw".into()
    }
}

#[derive(Debug)]
enum Row {
    Label(String),
    Separator,
    Toggle(String),
    /// Submenu of exclusive choices: label, selected, what picking it does.
    Choice(&'static str, Vec<(String, bool, Pick)>),
    /// "Never record <app>": adds the app to settings.json `ignore`.
    Never(String),
    Ignore,
    Auto(bool),
    Screen(bool),
    Notes,
    /// Opens the notes window on the Dictation tab.
    Dictation(String),
    Open(String, PathBuf),
}

const ACCESSIBILITY_SETTINGS: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Screen activity is on but macOS hasn't granted Accessibility (e.g. after a reinstall changed the
/// signature): nothing is recorded until it is.
fn screen_blocked() -> bool {
    SCREEN.load(Ordering::Relaxed) && !crate::screen::trusted(false)
}

fn entries(store: &Store) -> Vec<Row> {
    let recording = RECORDING.lock().unwrap().clone();
    let pending = PENDING.lock().unwrap().clone();
    let auto = AUTO.load(Ordering::Relaxed);
    let mut rows = vec![match (&recording, &pending) {
        (Some((app, _)), _) => Row::Label(format!("Recording: {app}")),
        (None, Some(app)) => Row::Label(format!("Call detected: {app}")),
        (None, None) if auto => Row::Label("Watching for calls (auto-record)".into()),
        (None, None) => Row::Label("Watching for calls (manual)".into()),
    }];
    let finishing = FINISHING.lock().unwrap().len();
    if finishing > 0 {
        rows.push(Row::Label(format!("Writing notes for {finishing} meeting(s)")));
    }
    rows.push(Row::Toggle(match (&recording, &pending) {
        (Some(_), _) => "Stop recording".into(),
        (None, Some(app)) => format!("Record call ({app})"),
        (None, None) => "Start recording".into(),
    }));
    if pending.is_some() && recording.is_none() {
        rows.push(Row::Ignore);
    }
    // A hand-started recording is labelled "manual" (watch_loop): there is no app to exclude.
    let app = pending
        .clone()
        .or_else(|| recording.as_ref().map(|r| r.0.clone()).filter(|a| a != "manual"));
    if let Some(app) = app {
        rows.push(Row::Never(app));
    }
    rows.push(Row::Auto(auto));
    rows.push(Row::Screen(SCREEN.load(Ordering::Relaxed)));
    if screen_blocked() {
        rows.push(Row::Open(
            "⚠︎ Screen activity paused: allow Accessibility…".into(),
            ACCESSIBILITY_SETTINGS.into(),
        ));
    }
    if NOTES_PROBLEM.lock().unwrap().is_some() {
        rows.push(Row::Open(
            "⚠︎ Notes are off: set up a notes model…".into(),
            crate::summarize::SETUP_URL.into(),
        ));
    }
    rows.push(Row::Separator);
    if let Some((cfg, _)) = self::store() {
        let engine = cfg.engine();
        let downloading = DOWNLOADING.lock().unwrap().clone();
        let speech = ENGINES
            .iter()
            .map(|(id, label)| {
                let note = if downloading.as_deref() == Some(*id) {
                    "  (downloading...)"
                } else if !crate::transcribe::installed(&cfg, id) {
                    "  (download on select)"
                } else {
                    ""
                };
                (format!("{label}{note}"), *id == engine, Pick::Engine(id.to_string()))
            })
            .collect();
        rows.push(Row::Choice("Speech model", speech));
        let url = cfg.llm_url();
        let mut servers: Vec<_> = crate::config::LLM_SERVERS
            .iter()
            .map(|(label, u)| (label.to_string(), *u == url, Pick::Server(u.to_string())))
            .collect();
        if !servers.iter().any(|s| s.1) {
            servers.push((url.clone(), true, Pick::Server(url)));
        }
        rows.push(Row::Choice("Notes server", servers));
        let current = cfg.llm_model();
        // Ollama lists `gemma4` as `gemma4:latest`.
        let same = |m: &str| m == current || m.strip_suffix(":latest") == Some(current.as_str());
        let mut models = SERVER_MODELS.lock().unwrap().clone();
        if !models.iter().any(|m| same(m)) {
            models.insert(0, current.clone());
        }
        let notes = models
            .into_iter()
            .map(|m| (m.clone(), same(&m), Pick::Llm(m)))
            .collect();
        rows.push(Row::Choice("Notes model", notes));
        let shortcut = crate::dictate::label(&crate::dictate::SHORTCUT.lock().unwrap());
        rows.push(Row::Dictation(if shortcut.is_empty() {
            "Set up dictation…".into()
        } else {
            format!("Dictation: hold {shortcut}…")
        }));
    }
    rows.push(Row::Notes);
    rows.push(Row::Separator);
    for m in store.list().into_iter().take(RECENT) {
        let when = m.id.get(5..15).unwrap_or(&m.id).replacen('_', " ", 1); // MM-DD HHMM
        rows.push(Row::Open(format!("{when}  {}", m.title), store.dir(&m.id)));
    }
    rows.push(Row::Separator);
    rows.push(Row::Open("Open meetings folder".into(), store.root.clone()));
    rows
}

#[derive(PartialEq)]
enum PillState {
    Hidden,
    Pending(String),
    Recording(String, u64),
}

fn pill_state() -> PillState {
    if let Some((app, since)) = &*RECORDING.lock().unwrap() {
        return PillState::Recording(app.clone(), since.elapsed().as_secs());
    }
    match &*PENDING.lock().unwrap() {
        Some(app) => PillState::Pending(app.clone()),
        None => PillState::Hidden,
    }
}

/// The notes page wants a full resend (a delete failed after the page dropped the row).
static RESEND: AtomicBool = AtomicBool::new(false);
/// Config and store of the running app, set once at startup.
static STORE: Mutex<Option<(Config, Store)>> = Mutex::new(None);

thread_local! {
    /// Dictation tab state last sent to the notes page.
    static DICTATION_SHOWN: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_store(cfg: &Config, store: &Store) {
    *STORE.lock().unwrap() = Some((cfg.clone(), store.clone()));
}

fn store() -> Option<(Config, Store)> {
    STORE.lock().unwrap().clone()
}

/// Opens a folder, file or URL with the system's default handler.
fn open(target: impl AsRef<Path>) {
    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(windows)]
    let mut cmd = Command::new("explorer");
    #[cfg(target_os = "linux")]
    let mut cmd = Command::new("xdg-open");
    let _ = cmd.arg(target.as_ref()).spawn();
}

/// A choice from one of the Speech model / Notes server / Notes model submenus.
fn pick(pick: Pick) {
    let Some((cfg, _)) = store() else { return };
    match pick {
        Pick::Engine(engine) => {
            let _ = crate::config::set_setting(&cfg.root, "engine", engine.clone().into());
            if !crate::transcribe::installed(&cfg, &engine) {
                // The next recording uses the new engine.
                download_models(cfg);
            }
        }
        Pick::Llm(model) => {
            let _ = crate::config::set_setting(&cfg.root, "llm_model", model.into());
        }
        Pick::Server(url) => {
            let _ = crate::config::set_setting(&cfg.root, "llm_url", url.into());
            SERVER_MODELS.lock().unwrap().clear();
        }
        Pick::Never(app) => set_recorded(&cfg, app, false),
    }
}

fn toggle_screen() {
    let on = !SCREEN.load(Ordering::Relaxed);
    SCREEN.store(on, Ordering::Relaxed);
    if let Some((cfg, _)) = store() {
        let _ = crate::config::set_setting(&cfg.root, "screen", on.into());
    }
}

/// Start/Stop recording, whichever applies.
fn toggle_recording() {
    let flag = if RECORDING.lock().unwrap().is_some() {
        &STOP
    } else {
        &START
    };
    flag.store(true, Ordering::Relaxed);
}

/// A message from the notes page (`window.webkit.messageHandlers.merkwerk.postMessage`): chat
/// questions, tag/title edits, deletes, Start/Stop recording, dictation shortcut.
fn handle_message(raw: &str) {
    let Ok(req) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    match req["action"].as_str() {
        Some("start") => return START.store(true, Ordering::Relaxed),
        Some("stop") => return STOP.store(true, Ordering::Relaxed),
        Some("record_shortcut") => {
            *crate::dictate::CAPTURE.lock().unwrap() = crate::dictate::Capture::Listening;
            return;
        }
        Some("cancel_shortcut") => {
            *crate::dictate::CAPTURE.lock().unwrap() = crate::dictate::Capture::Off;
            return;
        }
        Some("clear_shortcut") => {
            if let Some((cfg, _)) = store() {
                crate::dictate::set_shortcut(&cfg, vec![]);
            }
            return;
        }
        // Links in the page open in the browser, never inside the notes window.
        Some("open_url") => {
            if let Some(url) = req["url"].as_str().filter(|u| u.starts_with("https://")) {
                open(url);
            }
            return;
        }
        Some("summarize") => {
            if let (Some((cfg, store)), Some(id)) = (store(), req["target"].as_str()) {
                write_notes(cfg, store, id.to_string());
            }
            return;
        }
        Some("accessibility") => {
            #[cfg(target_os = "macos")]
            open(ACCESSIBILITY_SETTINGS);
            return;
        }
        _ => {}
    }
    let Some((cfg, store)) = store() else {
        return;
    };
    // "Never record <app>" or "Record <app> again" from a meeting's app label; the app
    // name comes from meta.json.
    if req["action"] == "never" || req["action"] == "record" {
        match store.load_editable(req["target"].as_str().unwrap_or_default()) {
            Ok(meta) => meta
                .app
                .into_iter()
                .for_each(|app| set_recorded(&cfg, app, req["action"] == "record")),
            Err(e) => eprintln!("never record: {e:#}"),
        }
        return;
    }
    // Tag edit: a small file write, done right here; the next refresh shows it everywhere.
    if let Some(tags) = req["tags"].as_array() {
        let tags: Vec<String> = tags.iter().filter_map(|t| Some(t.as_str()?.to_string())).collect();
        if let Err(e) = store.set_tags(req["target"].as_str().unwrap_or_default(), &tags) {
            eprintln!("tags: {e:#}");
        }
        return;
    }
    if let Some(title) = req["title"].as_str() {
        if let Err(e) = store.set_title(req["target"].as_str().unwrap_or_default(), title) {
            eprintln!("title: {e:#}");
        }
        return;
    }
    if req["delete"] == true {
        if let Err(e) = store.trash(req["target"].as_str().unwrap_or_default()) {
            eprintln!("delete: {e:#}");
            // The page already dropped the row; nothing changed on disk, so force a resend.
            RESEND.store(true, Ordering::Relaxed);
        }
        return;
    }
    // The LLM takes seconds: answer off the main thread, deliver on the next tick.
    std::thread::spawn(move || {
        let id = req["id"].as_u64().unwrap_or(0);
        let history: Vec<(String, String)> = req["history"]
            .as_array()
            .map(|h| {
                h.iter()
                    .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let target = req["target"].as_str().unwrap_or_default();
        let question = req["question"].as_str().unwrap_or_default();
        let answer = crate::ask::answer(&cfg, &store, target, &history, question).unwrap_or_else(|e| {
            eprintln!("ask: {e:#}");
            format!(
                "{} [How to set up a notes model]({})",
                crate::summarize::explain(&cfg, &e),
                crate::summarize::SETUP_URL
            )
        });
        let html = crate::notes::markdown(&answer);
        ANSWERS
            .lock()
            .unwrap()
            .push((id, serde_json::json!({"html": html, "text": answer}).to_string()));
    });
}

/// A fresh notes page and the fingerprint of the data in it.
fn notes_page(cfg: &Config, store: &Store) -> (String, u64) {
    let data = notes::data(cfg, store);
    let dictation = crate::dictate::page_state(cfg);
    let html = notes::page(&data, &dictation);
    DICTATION_SHOWN.with_borrow_mut(|s| *s = dictation);
    (html, notes::fingerprint(cfg, store))
}

/// Scripts that bring an open notes page up to date (a running transcript, finished notes, chat
/// answers); `shown` is the fingerprint of the data the page has. Called every tick.
fn notes_updates(cfg: &Config, store: &Store, shown: &mut u64) -> Vec<String> {
    let mut js = vec![format!(
        "window.merkwerkRecording({}); window.merkwerkScreenBlocked({}); window.merkwerkNotesServer({}); window.merkwerkIgnored({})",
        RECORDING.lock().unwrap().is_some(),
        screen_blocked(),
        serde_json::json!(*NOTES_PROBLEM.lock().unwrap()),
        serde_json::json!(cfg.ignore())
    )];
    if SHOW_DICTATION.swap(false, Ordering::Relaxed) {
        js.push("switchTab(\"dictation\")".into());
    }
    // The Dictation tab: sent only when it changed.
    let state = crate::dictate::page_state(cfg);
    if DICTATION_SHOWN.with_borrow(|s| *s != state) {
        js.push(format!("window.merkwerkDictation({state})"));
        DICTATION_SHOWN.with_borrow_mut(|s| *s = state);
    }
    for (id, answer) in ANSWERS.lock().unwrap().drain(..) {
        js.push(format!("window.merkwerkAnswer({id}, {answer})"));
    }
    if RESEND.swap(false, Ordering::Relaxed) {
        *shown = 0;
    }
    // Rebuild the page data only when one of its files changed.
    let fingerprint = notes::fingerprint(cfg, store);
    if fingerprint != *shown {
        js.push(format!("window.merkwerkUpdate({})", notes::data(cfg, store)));
        *shown = fingerprint;
    }
    js
}
