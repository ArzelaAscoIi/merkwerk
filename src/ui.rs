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

use crate::{config::Config, notes, state::AppState, store::Store};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, OnceLock},
};

/// What the UI callbacks reach: they have no arguments to carry it, so it is set once at startup.
struct Ctx {
    cfg: Config,
    store: Store,
    state: Arc<AppState>,
}

static CTX: OnceLock<Ctx> = OnceLock::new();

fn set_store(cfg: &Config, store: &Store, state: &Arc<AppState>) {
    let _ = CTX.set(Ctx {
        cfg: cfg.clone(),
        store: store.clone(),
        state: state.clone(),
    });
}

fn store() -> Option<(Config, Store)> {
    CTX.get().map(|c| (c.cfg.clone(), c.store.clone()))
}

#[expect(clippy::expect_used, reason = "every UI entry point calls set_store first")]
fn state() -> &'static Arc<AppState> {
    &CTX.get().expect("ui context set at startup").state
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
        && state()
            .recording
            .lock()
            .as_ref()
            .is_some_and(|r| notes::app_name(&r.0).eq_ignore_ascii_case(&app))
    {
        state().stop.set(true);
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
    state().auto_record.set(on);
    if let Some((cfg, _)) = store() {
        let _ = crate::config::set_setting(&cfg.root, "auto_record", on.into());
    }
}

/// Writes a meeting's notes again in the background (the Write notes button), like after a recording.
fn write_notes(cfg: Config, store: Store, id: String) {
    let mut finishing = state().finishing.lock();
    if finishing.contains(&id) || store.load(&id).is_ok_and(|m| store.live(&m)) {
        return;
    }
    finishing.push(id.clone());
    std::thread::spawn(move || {
        match crate::session::finalize_with_posthog(&cfg, &store, &id, state().posthog.get()) {
            Ok(()) if store.read(&id, "summary.md").is_some() => {
                crate::posthog::capture(state().posthog.get(), "meeting_notes_generated");
            }
            Ok(()) => {}
            Err(e) => eprintln!("notes for {id}: {e:#}"),
        }
        state().finishing.lock().retain(|f| *f != id);
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
            *state().notes_problem.lock() = match (&url, &names) {
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
            state().notes_reachable.set(names.is_some());
            // Empty on failure so a previous server's models don't linger.
            let mut names = names.unwrap_or_default();
            names.sort();
            *state().server_models.lock() = names;
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    });
}

fn title() -> String {
    if let Some((_, since)) = &*state().recording.lock() {
        let s = since.elapsed().as_secs();
        return format!("● {:02}:{:02}", s / 60, s % 60);
    }
    if !state().finishing.lock().is_empty() {
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
    state().screen.is_set() && !crate::screen::trusted(false)
}

fn entries(store: &Store) -> Vec<Row> {
    let recording = state().recording.lock().clone();
    let pending = state().pending.lock().clone();
    let auto = state().auto_record.is_set();
    let mut rows = vec![match (&recording, &pending) {
        (Some((app, _)), _) => Row::Label(format!("Recording: {app}")),
        (None, Some(app)) => Row::Label(format!("Call detected: {app}")),
        (None, None) if auto => Row::Label("Watching for calls (auto-record)".into()),
        (None, None) => Row::Label("Watching for calls (manual)".into()),
    }];
    let finishing = state().finishing.lock().len();
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
    rows.push(Row::Screen(state().screen.is_set()));
    if screen_blocked() {
        rows.push(Row::Open(
            "⚠︎ Screen activity paused: allow Accessibility…".into(),
            ACCESSIBILITY_SETTINGS.into(),
        ));
    }
    if state().notes_problem.lock().is_some() {
        rows.push(Row::Open(
            "⚠︎ Notes are off: set up a notes model…".into(),
            crate::summarize::SETUP_URL.into(),
        ));
    }
    rows.push(Row::Separator);
    if let Some((cfg, _)) = self::store() {
        let engine = cfg.engine();
        let downloading = state().downloading.lock().clone();
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
        let mut models = state().server_models.lock().clone();
        if !models.iter().any(|m| same(m)) {
            models.insert(0, current.clone());
        }
        let notes = models
            .into_iter()
            .map(|m| (m.clone(), same(&m), Pick::Llm(m)))
            .collect();
        rows.push(Row::Choice("Notes model", notes));
        let shortcut = crate::dictate::label(&state().shortcut.lock());
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
    if let Some((app, since)) = &*state().recording.lock() {
        return PillState::Recording(app.clone(), since.elapsed().as_secs());
    }
    match &*state().pending.lock() {
        Some(app) => PillState::Pending(app.clone()),
        None => PillState::Hidden,
    }
}

thread_local! {
    /// Dictation tab state last sent to the notes page.
    static DICTATION_SHOWN: RefCell<String> = const { RefCell::new(String::new()) };
    /// "Get started" state last sent to the notes page.
    static SETUP_SHOWN: RefCell<String> = const { RefCell::new(String::new()) };
}

/// What the "Get started" page shows: speech model download, notes model, the audio test.
fn setup_state(cfg: &Config) -> String {
    use crate::{audio::Source, setup};
    let engine = cfg.engine();
    let speech_ready = crate::transcribe::installed(cfg, &engine);
    let downloading = state().downloading.lock().is_some();
    let ollama = cfg.llm_url() == crate::config::LLM_SERVERS[0].1;
    let reachable = state().notes_reachable.is_set();
    let test = state().audio_test.lock().clone();
    let fix = |side, heard: Option<bool>| {
        (heard == Some(false)).then(|| {
            let (url, text) = setup::fix(side);
            serde_json::json!({ "url": url, "text": text })
        })
    };
    serde_json::json!({
        "speech": {
            "ready": speech_ready,
            "downloading": downloading,
            "percent": state().download_percent(),
        },
        "notes": {
            "problem": *state().notes_problem.lock(),
            "model": cfg.llm_model(),
            "server": crate::summarize::server_name(cfg),
            // Ollama runs but lacks the model: one click downloads it.
            "can_pull": ollama && reachable,
            // Ollama isn't running (or installed): link to its download page.
            "no_ollama": ollama && !reachable,
            "pull": *state().model_pull.lock(),
        },
        "test": {
            "running": test.running,
            "me": test.me,
            "them": test.them,
            "error": test.error,
            "fix_me": fix(Source::Me, test.me),
            "fix_them": fix(Source::Them, test.them),
        },
    })
    .to_string()
    .replace('<', "\\u003c")
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
                crate::models::download_in_background(cfg, state().clone());
            }
        }
        Pick::Llm(model) => {
            let _ = crate::config::set_setting(&cfg.root, "llm_model", model.into());
        }
        Pick::Server(url) => {
            let _ = crate::config::set_setting(&cfg.root, "llm_url", url.into());
            state().server_models.lock().clear();
        }
        Pick::Never(app) => set_recorded(&cfg, app, false),
    }
}

fn toggle_screen() {
    let on = !state().screen.is_set();
    state().screen.set(on);
    if let Some((cfg, _)) = store() {
        let _ = crate::config::set_setting(&cfg.root, "screen", on.into());
    }
}

/// Start/Stop recording, whichever applies.
fn toggle_recording() {
    let st = state();
    if st.is_recording() {
        st.stop.set(true);
    } else {
        st.start.set(true);
    }
}

/// A message from the notes page (`window.webkit.messageHandlers.merkwerk.postMessage`): chat
/// questions, tag/title edits, deletes, Start/Stop recording, dictation shortcut.
fn handle_message(raw: &str) {
    let Ok(req) = serde_json::from_str::<serde_json::Value>(raw) else {
        return;
    };
    match req["action"].as_str() {
        Some("start") => return state().start.set(true),
        Some("stop") => return state().stop.set(true),
        Some("record_shortcut") => {
            *state().capture.lock() = crate::dictate::Capture::Listening;
            return;
        }
        Some("cancel_shortcut") => {
            *state().capture.lock() = crate::dictate::Capture::Off;
            return;
        }
        Some("clear_shortcut") => {
            if let Some((cfg, _)) = store() {
                crate::dictate::set_shortcut(&cfg, state(), vec![]);
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
        Some("test_audio") => return crate::setup::start_test(state()),
        Some("pull_model") => {
            if let Some((cfg, _)) = store() {
                crate::setup::pull_model(&cfg, state());
            }
            return;
        }
        Some("retry_download") => {
            if let Some((cfg, _)) = store() {
                crate::models::download_in_background(cfg, state().clone());
            }
            return;
        }
        // System settings pages for a permission; only the ones setup::fix names.
        Some("open_settings") => {
            if let Some(url) = req["url"]
                .as_str()
                .filter(|u| u.starts_with("x-apple.systempreferences:") || u.starts_with("ms-settings:"))
            {
                open(url);
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
            state().resend.set(true);
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
        let answer = match crate::ask::answer_with_posthog(
            &cfg,
            &store,
            target,
            &history,
            question,
            state().posthog.get(),
        ) {
            Ok(answer) => {
                crate::posthog::capture(state().posthog.get(), "meeting_question_answered");
                answer
            }
            Err(e) => {
                eprintln!("ask: {e:#}");
                format!(
                    "{} [How to set up a notes model]({})",
                    crate::summarize::explain(&cfg, &e),
                    crate::summarize::SETUP_URL
                )
            }
        };
        let html = crate::notes::markdown(&answer);
        state()
            .answers
            .lock()
            .push((id, serde_json::json!({"html": html, "text": answer}).to_string()));
    });
}

/// A fresh notes page and the fingerprint of the data in it.
fn notes_page(cfg: &Config, store: &Store) -> (String, u64) {
    let data = notes::data(cfg, store, state());
    let dictation = crate::dictate::page_state(cfg, state());
    let html = notes::page(&data, &dictation);
    DICTATION_SHOWN.with_borrow_mut(|s| *s = dictation);
    SETUP_SHOWN.with_borrow_mut(String::clear);
    (html, notes::fingerprint(cfg, store, state()))
}

/// Scripts that bring an open notes page up to date (a running transcript, finished notes, chat
/// answers); `shown` is the fingerprint of the data the page has. Called every tick.
fn notes_updates(cfg: &Config, store: &Store, shown: &mut u64) -> Vec<String> {
    let mut js = vec![format!(
        "window.merkwerkRecording({}); window.merkwerkScreenBlocked({}); window.merkwerkNotesServer({}); window.merkwerkIgnored({})",
        state().recording.lock().is_some(),
        screen_blocked(),
        serde_json::json!(*state().notes_problem.lock()),
        serde_json::json!(cfg.ignore())
    )];
    if state().show_dictation.take() {
        js.push("switchTab(\"dictation\")".into());
    }
    // The Dictation tab: sent only when it changed.
    let dictation = crate::dictate::page_state(cfg, state());
    if DICTATION_SHOWN.with_borrow(|s| *s != dictation) {
        js.push(format!("window.merkwerkDictation({dictation})"));
        DICTATION_SHOWN.with_borrow_mut(|s| *s = dictation);
    }
    // The "Get started" page: sent only when it changed.
    let setup = setup_state(cfg);
    if SETUP_SHOWN.with_borrow(|s| *s != setup) {
        js.push(format!("window.merkwerkSetup({setup})"));
        SETUP_SHOWN.with_borrow_mut(|s| *s = setup);
    }
    for (id, answer) in state().answers.lock().drain(..) {
        js.push(format!("window.merkwerkAnswer({id}, {answer})"));
    }
    if state().resend.take() {
        *shown = 0;
    }
    // Rebuild the page data only when one of its files changed.
    let fingerprint = notes::fingerprint(cfg, store, state());
    if fingerprint != *shown {
        js.push(format!("window.merkwerkUpdate({})", notes::data(cfg, store, state())));
        *shown = fingerprint;
    }
    js
}
