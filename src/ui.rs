//! Menu bar indicator: `mw` while watching, `● 03:12` while recording, `◌` while writing notes.
//! The same status shows as a Dock badge (`REC`), since a full menu bar hides items behind the notch;
//! the Dock icon reopens the notes window and its right-click menu mirrors the menu bar menu.
//! The menu starts/stops a recording by hand (in-person talks) and lists recent meetings;
//! clicking one opens its folder.

use crate::{config::Config, notes, store::Store};
use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
    sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSBox, NSBoxType, NSButton, NSColor, NSControlSize, NSControlStateValueOff, NSControlStateValueOn, NSEvent, NSFont,
    NSFontWeightMedium, NSImage, NSMenu, NSMenuItem, NSPanel, NSRequestUserAttentionType, NSScreen, NSStatusBar,
    NSStatusItem, NSStatusWindowLevel, NSTextAlignment, NSTextField, NSTitlePosition, NSVariableStatusItemLength,
    NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString, NSTimer, ns_string};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView, WKWebViewConfiguration,
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
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
/// Models offered by the notes server, refreshed in the background.
static SERVER_MODELS: Mutex<Vec<String>> = Mutex::new(vec![]);
/// Chat answers waiting to be handed to the page: (request id, answer HTML).
static ANSWERS: Mutex<Vec<(u64, String)>> = Mutex::new(vec![]);
/// Speech engine whose model is being downloaded.
static DOWNLOADING: Mutex<Option<String>> = Mutex::new(None);

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

/// The open notes window and the fingerprint of the data it shows.
type NotesWindow = (Retained<NSWindow>, Retained<WKWebView>, u64);

thread_local! {
    /// Folder behind each clickable menu item, indexed by the item's tag.
    static PATHS: RefCell<Vec<PathBuf>> = const { RefCell::new(vec![]) };
    /// Model choice behind each model menu item, indexed by the item's tag.
    static PICKS: RefCell<Vec<Pick>> = const { RefCell::new(vec![]) };
    static STORE: RefCell<Option<(Config, Store)>> = const { RefCell::new(None) };
    /// Latest status menu, also served as the Dock icon's right-click menu.
    static DOCK_MENU: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) };
    static NOTES: RefCell<Option<NotesWindow>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MerkwerkMenuHandler"]
    struct Handler;

    impl Handler {
        #[unsafe(method(open:))]
        fn open(&self, item: &NSMenuItem) {
            let path = PATHS.with_borrow(|p| p.get(item.tag() as usize).cloned());
            if let Some(path) = path {
                let _ = Command::new("open").arg(path).spawn();
            }
        }

        #[unsafe(method(notes:))]
        fn notes(&self, _item: &NSMenuItem) {
            open_notes(MainThreadMarker::from(self));
        }

        #[unsafe(method(pick:))]
        fn pick(&self, item: &NSMenuItem) {
            let pick = PICKS.with_borrow(|p| p.get(item.tag() as usize).cloned());
            let Some((cfg, _)) = STORE.with_borrow(|s| s.clone()) else { return };
            match pick {
                Some(Pick::Engine(engine)) => {
                    let _ = crate::config::set_setting(&cfg.root, "engine", engine.clone().into());
                    if !crate::transcribe::installed(&cfg, &engine) {
                        // Download in the background; the next recording uses the new engine.
                        *DOWNLOADING.lock().unwrap() = Some(engine);
                        std::thread::spawn(move || {
                            if let Err(e) = crate::transcribe::ensure_models(&cfg) {
                                eprintln!("model download: {e:#}");
                            }
                            *DOWNLOADING.lock().unwrap() = None;
                        });
                    }
                }
                Some(Pick::Llm(model)) => {
                    let _ = crate::config::set_setting(&cfg.root, "llm_model", model.into());
                }
                Some(Pick::Server(url)) => {
                    let _ = crate::config::set_setting(&cfg.root, "llm_url", url.into());
                    SERVER_MODELS.lock().unwrap().clear();
                }
                None => {}
            }
        }

        #[unsafe(method(ignore:))]
        fn ignore(&self, _sender: &NSObject) {
            IGNORE.store(true, Ordering::Relaxed);
        }

        #[unsafe(method(screen:))]
        fn screen(&self, _sender: &NSObject) {
            let on = !SCREEN.load(Ordering::Relaxed);
            SCREEN.store(on, Ordering::Relaxed);
            if let Some((cfg, _)) = STORE.with_borrow(|s| s.clone()) {
                let _ = crate::config::set_setting(&cfg.root, "screen", on.into());
            }
        }

        #[unsafe(method(auto:))]
        fn auto(&self, _sender: &NSObject) {
            let on = !AUTO.load(Ordering::Relaxed);
            AUTO.store(on, Ordering::Relaxed);
            if let Some((cfg, _)) = STORE.with_borrow(|s| s.clone()) {
                let _ = crate::config::set_setting(&cfg.root, "auto_record", on.into());
            }
        }

        /// The pill's Stop: only ever stops, so a late second click can't start a new recording.
        #[unsafe(method(stop:))]
        fn stop(&self, _sender: &NSObject) {
            STOP.store(true, Ordering::Relaxed);
        }

        /// The pill's Record: only ever starts, so a second click can't stop what the first started.
        #[unsafe(method(record:))]
        fn record(&self, _sender: &NSObject) {
            START.store(true, Ordering::Relaxed);
        }

        #[unsafe(method(toggle:))]
        fn toggle(&self, _sender: &NSObject) {
            let flag = if RECORDING.lock().unwrap().is_some() { &STOP } else { &START };
            flag.store(true, Ordering::Relaxed);
        }
    }
);

define_class!(
    /// Receives chat questions, tag/title edits, deletes and Start/Stop recording from the notes
    /// page (`window.webkit.messageHandlers.merkwerk`).
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MerkwerkChatBridge"]
    struct ChatBridge;

    unsafe impl NSObjectProtocol for ChatBridge {}

    unsafe impl WKScriptMessageHandler for ChatBridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            let body = unsafe { message.body() };
            let Some(raw) = body.downcast_ref::<NSString>().map(|s| s.to_string()) else {
                return;
            };
            let Ok(req) = serde_json::from_str::<serde_json::Value>(&raw) else {
                return;
            };
            match req["action"].as_str() {
                Some("start") => return START.store(true, Ordering::Relaxed),
                Some("stop") => return STOP.store(true, Ordering::Relaxed),
                Some("accessibility") => {
                    let _ = Command::new("open").arg(ACCESSIBILITY_SETTINGS).spawn();
                    return;
                }
                _ => {}
            }
            let Some((cfg, store)) = STORE.with_borrow(|s| s.clone()) else {
                return;
            };
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
                    NOTES.with_borrow_mut(|n| n.as_mut().map(|(_, _, shown)| *shown = 0));
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
                let answer = crate::ask::answer(&cfg, &store, target, &history, question)
                    .unwrap_or_else(|e| format!("Could not answer: {e:#}"));
                let html = crate::notes::markdown(&answer);
                ANSWERS
                    .lock()
                    .unwrap()
                    .push((id, serde_json::json!({"html": html, "text": answer}).to_string()));
            });
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MerkwerkAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        /// Clicking the Dock icon brings the notes window back.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            open_notes(MainThreadMarker::from(self));
            true
        }

        #[unsafe(method(applicationDockMenu:))]
        fn dock_menu(&self, _app: &NSApplication) -> *mut NSMenu {
            DOCK_MENU.with_borrow(|m| {
                m.as_ref()
                    .map_or(std::ptr::null_mut(), |m| Retained::as_ptr(m).cast_mut())
            })
        }
    }
);

#[derive(Debug, Clone)]
enum Pick {
    Engine(String),
    Llm(String),
    Server(String),
}

/// Keeps SERVER_MODELS current (every 10 s) from the notes server's `/v1/models`.
fn watch_models(cfg: &Config) {
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        loop {
            // Re-read each time: the menu can switch servers.
            let names: Option<Vec<String>> = cfg.models_url().and_then(|url| {
                let mut req = ureq::get(&url);
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
            // Empty on failure so a previous server's models don't linger.
            let mut names = names.unwrap_or_default();
            names.sort();
            *SERVER_MODELS.lock().unwrap() = names;
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    });
}

/// Run the menu bar on the main thread. Never returns.
pub fn run(cfg: Config, store: Store) -> ! {
    let mtm = MainThreadMarker::new().expect("ui must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate: Retained<AppDelegate> = unsafe { msg_send![mtm.alloc::<AppDelegate>(), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    app.setMainMenu(Some(&main_menu(mtm)));
    set_icon(mtm, &app);
    STORE.with_borrow_mut(|s| *s = Some((cfg.clone(), store.clone())));
    watch_models(&cfg);
    open_notes(mtm);

    let handler: Retained<Handler> = unsafe { msg_send![mtm.alloc::<Handler>(), init] };
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let pill = std::rc::Rc::new(Pill::new(mtm, &handler));
    let meter = pill.clone();
    let level = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| meter.level());
    let _level_timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.1, true, &level) };
    let mut last_menu = String::new();
    let mut last_badge = String::new();
    let mut last_state = PillState::Hidden;
    let mut ticks: u64 = 0;
    let mut last_title = String::new();
    let tick = move || {
        // Setting the status item title redraws the menu bar item, so only on change.
        let now = title();
        if now != last_title {
            set_title(&item, mtm, &now);
            last_title = now;
        }
        let state = pill_state();
        // Bounce the Dock icon once when a call is detected or a recording starts.
        let kind = |s: &PillState| std::mem::discriminant(s);
        if kind(&state) != kind(&last_state) && state != PillState::Hidden {
            NSApplication::sharedApplication(mtm)
                .requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
        }
        pill.show(mtm, &state);
        last_state = state;
        let badge = badge();
        if badge != last_badge {
            let tile = NSApplication::sharedApplication(mtm).dockTile();
            tile.setBadgeLabel((!badge.is_empty()).then(|| NSString::from_str(&badge)).as_deref());
            last_badge = badge;
        }
        refresh_notes(&cfg, &store);
        // The menu reads meeting files; every 5 s is plenty. Rebuild only on change so an open
        // menu isn't yanked away.
        ticks += 1;
        if ticks % 5 != 1 {
            return;
        }
        let entries = entries(&store);
        let key = format!("{entries:?}");
        if key != last_menu {
            let menu = menu(mtm, &handler, &entries);
            item.setMenu(Some(&menu));
            DOCK_MENU.with_borrow_mut(|m| *m = Some(menu));
            last_menu = key;
        }
    };
    let tick = RefCell::new(tick);
    let block = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| (tick.borrow_mut())());
    block.call((std::ptr::NonNull::dangling(),)); // first paint without waiting a second
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &block) };
    app.run();
    std::process::exit(0)
}

/// The Dock icon: assets/logo.png (rendered from logo.svg), embedded in the binary.
fn set_icon(mtm: MainThreadMarker, app: &NSApplication) {
    let data = NSData::with_bytes(include_bytes!("../assets/logo.png"));
    if let Some(image) = NSImage::initWithData(mtm.alloc(), &data) {
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

/// Dock badge: `REC` while recording, `…` while writing notes, nothing otherwise.
fn badge() -> String {
    if RECORDING.lock().unwrap().is_some() {
        "REC".into()
    } else if !FINISHING.lock().unwrap().is_empty() {
        "…".into()
    } else {
        String::new()
    }
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

fn set_title(item: &NSStatusItem, mtm: MainThreadMarker, title: &str) {
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str(title));
    }
}

#[derive(Debug)]
enum Row {
    Label(String),
    Separator,
    Toggle(String),
    /// Submenu of exclusive choices: label, selected, what picking it does.
    Choice(&'static str, Vec<(String, bool, Pick)>),
    Ignore,
    Auto(bool),
    Screen(bool),
    Notes,
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
    rows.push(Row::Auto(auto));
    rows.push(Row::Screen(SCREEN.load(Ordering::Relaxed)));
    if screen_blocked() {
        rows.push(Row::Open(
            "⚠︎ Screen activity paused: allow Accessibility…".into(),
            ACCESSIBILITY_SETTINGS.into(),
        ));
    }
    rows.push(Row::Separator);
    if let Some((cfg, _)) = STORE.with_borrow(|s| s.clone()) {
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

fn menu(mtm: MainThreadMarker, handler: &Handler, rows: &[Row]) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    let mut paths = vec![];
    let mut picks = vec![];
    let add = |label: &str, action: Option<objc2::runtime::Sel>, key: &NSString| {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(label), action, key)
        };
        menu.addItem(&item);
        item
    };
    for row in rows {
        match row {
            Row::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
            Row::Label(label) => drop(add(label, None, ns_string!(""))),
            Row::Toggle(label) => unsafe { add(label, Some(sel!(toggle:)), ns_string!("r")).setTarget(Some(handler)) },
            Row::Choice(title, options) => {
                let sub = NSMenu::new(mtm);
                for (label, selected, pick) in options {
                    let item = unsafe {
                        NSMenuItem::initWithTitle_action_keyEquivalent(
                            mtm.alloc(),
                            &NSString::from_str(label),
                            Some(sel!(pick:)),
                            ns_string!(""),
                        )
                    };
                    unsafe { item.setTarget(Some(handler)) };
                    item.setState(if *selected {
                        NSControlStateValueOn
                    } else {
                        NSControlStateValueOff
                    });
                    item.setTag(picks.len() as isize);
                    picks.push(pick.clone());
                    sub.addItem(&item);
                }
                let holder = add(title, None, ns_string!(""));
                holder.setSubmenu(Some(&sub));
            }
            Row::Screen(on) => {
                let item = add("Record screen activity", Some(sel!(screen:)), ns_string!(""));
                unsafe { item.setTarget(Some(handler)) };
                item.setState(if *on {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
            }
            Row::Ignore => unsafe {
                add("Ignore this call", Some(sel!(ignore:)), ns_string!("")).setTarget(Some(handler))
            },
            Row::Auto(on) => {
                let item = add("Auto-record calls", Some(sel!(auto:)), ns_string!(""));
                unsafe { item.setTarget(Some(handler)) };
                item.setState(if *on {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
            }
            Row::Notes => unsafe { add("Open notes", Some(sel!(notes:)), ns_string!("o")).setTarget(Some(handler)) },
            Row::Open(label, path) => {
                let item = add(label, Some(sel!(open:)), ns_string!(""));
                unsafe { item.setTarget(Some(handler)) };
                item.setTag(paths.len() as isize);
                paths.push(path.clone());
            }
        }
    }
    add("Quit", Some(sel!(terminate:)), ns_string!("q")); // NSApp handles terminate: via the responder chain
    PATHS.with_borrow_mut(|p| *p = paths);
    PICKS.with_borrow_mut(|p| *p = picks);
    menu
}

/// Just the notes window, as its own app (`merkwerk notes`); exits when the window closes.
pub fn run_notes(cfg: Config, store: Store) -> ! {
    let mtm = MainThreadMarker::new().expect("ui must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.setMainMenu(Some(&main_menu(mtm)));
    set_icon(mtm, &app);
    STORE.with_borrow_mut(|s| *s = Some((cfg.clone(), store.clone())));
    open_notes(mtm);
    let block = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
        let open = NOTES.with_borrow(|n| n.as_ref().is_some_and(|(w, _, _)| w.isVisible()));
        if !open {
            std::process::exit(0);
        }
        refresh_notes(&cfg, &store);
    });
    let _timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0, true, &block) };
    app.run();
    std::process::exit(0)
}

/// Show the notes window, creating it on first use.
fn open_notes(mtm: MainThreadMarker) {
    let Some((cfg, store)) = STORE.with_borrow(|s| s.clone()) else {
        return;
    };
    let data = notes::data(&cfg, &store);
    NOTES.with_borrow_mut(|slot| {
        let (window, web, shown) = slot.get_or_insert_with(|| {
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1180.0, 780.0));
            let style = NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable;
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    mtm.alloc(),
                    rect,
                    style,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(ns_string!("merkwerk"));
            window.setMinSize(NSSize::new(640.0, 420.0));
            let config = unsafe { WKWebViewConfiguration::new(mtm) };
            let bridge: Retained<ChatBridge> = unsafe { msg_send![mtm.alloc::<ChatBridge>(), init] };
            unsafe {
                config
                    .userContentController()
                    .addScriptMessageHandler_name(ProtocolObject::from_ref(&*bridge), ns_string!("merkwerk"))
            };
            let web = unsafe { WKWebView::initWithFrame_configuration(mtm.alloc(), rect, &config) };
            window.setContentView(Some(&web));
            window.center();
            (window, web, 0)
        });
        // A fresh page on every open; live updates in between go through refresh_notes.
        unsafe { web.loadHTMLString_baseURL(&NSString::from_str(&notes::page(&data)), None) };
        *shown = notes::fingerprint(&cfg, &store);
        window.makeKeyAndOrderFront(None);
    });
    NSApplication::sharedApplication(mtm).activate();
}

/// Push new data into the open window (a running transcript, finished notes). Called every tick.
fn refresh_notes(cfg: &Config, store: &Store) {
    NOTES.with_borrow_mut(|slot| {
        let Some((window, web, shown)) = slot.as_mut() else {
            return;
        };
        if !window.isVisible() {
            // Closed (not minimized): drop the window so its WebKit content process exits and
            // frees its memory; the next open builds a fresh one.
            if !window.isMiniaturized() {
                *slot = None;
            }
            return;
        }
        let js = format!(
            "window.merkwerkRecording({}); window.merkwerkScreenBlocked({})",
            RECORDING.lock().unwrap().is_some(),
            screen_blocked()
        );
        unsafe { web.evaluateJavaScript_completionHandler(&NSString::from_str(&js), None) };
        for (id, answer) in ANSWERS.lock().unwrap().drain(..) {
            let js = format!("window.merkwerkAnswer({id}, {answer})");
            unsafe { web.evaluateJavaScript_completionHandler(&NSString::from_str(&js), None) };
        }
        // Rebuild the page data only when one of its files changed.
        let fingerprint = notes::fingerprint(cfg, store);
        if fingerprint != *shown {
            let js = format!("window.merkwerkUpdate({})", notes::data(cfg, store));
            unsafe { web.evaluateJavaScript_completionHandler(&NSString::from_str(&js), None) };
            *shown = fingerprint;
        }
    });
}

/// Without a main menu, copy, select-all and close-window shortcuts do nothing in the window.
fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let item = |title: &str, action, key: &NSString| unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), Some(action), key)
    };
    let bar = NSMenu::new(mtm);
    let add_menu = |title: &str, items: Vec<Retained<NSMenuItem>>| {
        let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title));
        for i in items {
            menu.addItem(&i);
        }
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(&menu));
        bar.addItem(&holder);
    };
    add_menu("merkwerk", vec![item("Quit", sel!(terminate:), ns_string!("q"))]);
    add_menu(
        "Edit",
        vec![
            item("Copy", sel!(copy:), ns_string!("c")),
            item("Paste", sel!(paste:), ns_string!("v")),
            item("Select All", sel!(selectAll:), ns_string!("a")),
        ],
    );
    add_menu("Window", vec![item("Close", sel!(performClose:), ns_string!("w"))]);
    bar
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

/// Floating pill at the top of the screen, on every desktop: the recording indicator and the
/// Record/Ignore prompt for a detected call. Unlike the menu bar item, the notch can't hide it.
/// While recording it shrinks to a breathing dot and the time; hovering it shows the app and Stop.
struct Pill {
    panel: Retained<NSPanel>,
    dot: Retained<NSTextField>,
    label: Retained<NSTextField>,
    primary: Retained<NSButton>,
    secondary: Retained<NSButton>,
    compact: Cell<bool>,
    /// Hover expands only once the mouse has left the pill since recording began, so the click on
    /// Record can't turn into a click on Stop.
    armed: Cell<bool>,
    /// Label text for the compact and the full pill.
    text: RefCell<(String, String)>,
    born: Instant,
}

const PILL_W: f64 = 320.0;
const PILL_H: f64 = 28.0;
const PILL_COMPACT_W: f64 = 76.0;

/// The notes page's dark-mode accent, #3cc497; the pill is always dark.
fn accent() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0x3c as f64 / 255.0, 0xc4 as f64 / 255.0, 0x97 as f64 / 255.0, 1.0)
}

impl Pill {
    fn new(mtm: MainThreadMarker, handler: &Handler) -> Self {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PILL_W, PILL_H));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setLevel(NSStatusWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        panel.setHidesOnDeactivate(false);
        panel.setOpaque(false);
        panel.setHasShadow(true);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        // Drag it anywhere, on any screen; AppKit saves where it was left under this name.
        panel.setMovableByWindowBackground(true);
        panel.setFrameAutosaveName(ns_string!("pill"));

        let root = NSView::initWithFrame(mtm.alloc(), rect);
        let background = NSBox::initWithFrame(mtm.alloc(), rect);
        background.setBoxType(NSBoxType::Custom);
        background.setTitlePosition(NSTitlePosition::NoTitle);
        background.setBorderWidth(0.0);
        background.setCornerRadius(PILL_H / 2.0);
        background.setFillColor(&NSColor::colorWithWhite_alpha(0.09, 0.94));
        background.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        root.addSubview(&background);

        let text = |s: &str, x: f64, w: f64, color: &NSColor| {
            let t = NSTextField::labelWithString(&NSString::from_str(s), mtm);
            t.setFrame(NSRect::new(NSPoint::new(x, 6.0), NSSize::new(w, 16.0)));
            t.setTextColor(Some(color));
            t.setFont(Some(&NSFont::systemFontOfSize_weight(12.0, unsafe {
                NSFontWeightMedium
            })));
            root.addSubview(&t);
            t
        };
        let dot = text("●", 7.0, 16.0, &accent());
        dot.setAlignment(NSTextAlignment::Center);
        let label = text("", 26.0, 160.0, &NSColor::colorWithWhite_alpha(0.95, 1.0));
        let button = |action, x: f64, w: f64, size: NSControlSize| {
            let b =
                unsafe { NSButton::buttonWithTitle_target_action(ns_string!(""), Some(handler), Some(action), mtm) };
            b.setControlSize(size);
            let h = if size == NSControlSize::Small { 22.0 } else { PILL_H };
            b.setFrame(NSRect::new(NSPoint::new(x, (PILL_H - h) / 2.0), NSSize::new(w, h)));
            b.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            root.addSubview(&b);
            b
        };
        // Record is the big one: it is what a detected call is shown for.
        let primary = button(sel!(record:), PILL_W - 100.0, 96.0, NSControlSize::Regular);
        let secondary = button(sel!(ignore:), PILL_W - 160.0, 56.0, NSControlSize::Small);
        panel.setContentView(Some(&root));
        Pill {
            panel,
            dot,
            label,
            primary,
            secondary,
            compact: Cell::new(false),
            armed: Cell::new(false),
            text: RefCell::default(),
            born: Instant::now(),
        }
    }

    /// The dot breathes slowly and lights up with what the mic or system audio hears. Also expands
    /// the recording pill while the mouse is over it.
    fn level(&self) {
        let peak = f32::from_bits(crate::audio::LEVEL.swap(0, Ordering::Relaxed));
        let recording = RECORDING.lock().unwrap().is_some();
        let alpha = if recording {
            let breath = 0.45 + 0.2 * (self.born.elapsed().as_secs_f32() * std::f32::consts::PI).sin();
            breath.max(0.25 + 1.5 * peak.sqrt()).min(1.0)
        } else {
            1.0
        };
        self.dot.setAlphaValue(alpha as f64);
        if recording && self.panel.isVisible() {
            let f = self.panel.frame();
            let m = NSEvent::mouseLocation();
            let inside = m.x >= f.origin.x
                && m.x <= f.origin.x + f.size.width
                && m.y >= f.origin.y
                && m.y <= f.origin.y + f.size.height;
            if !inside {
                self.armed.set(true);
            }
            self.set_compact(!(inside && self.armed.get()));
        }
    }

    fn set_label(&self) {
        let compact = self.compact.get();
        let text = self.text.borrow();
        self.label
            .setStringValue(&NSString::from_str(if compact { &text.0 } else { &text.1 }));
        let width = if compact {
            PILL_COMPACT_W - 32.0
        } else if self.secondary.isHidden() {
            190.0
        } else {
            130.0
        };
        self.label
            .setFrame(NSRect::new(NSPoint::new(26.0, 6.0), NSSize::new(width, 16.0)));
    }

    /// Resizes around the same center; the compact pill shows only the dot and the time.
    fn set_compact(&self, compact: bool) {
        if compact == self.compact.get() {
            return;
        }
        self.compact.set(compact);
        self.set_label();
        self.primary.setHidden(compact);
        let width = if compact { PILL_COMPACT_W } else { PILL_W };
        let f = self.panel.frame();
        let x = f.origin.x + (f.size.width - width) / 2.0;
        self.panel.setFrame_display(
            NSRect::new(NSPoint::new(x, f.origin.y), NSSize::new(width, PILL_H)),
            true,
        );
        self.panel.invalidateShadow();
    }

    fn show(&self, mtm: MainThreadMarker, state: &PillState) {
        let (short, full, primary, secondary, color) = match state {
            PillState::Hidden => {
                self.panel.orderOut(None);
                return;
            }
            PillState::Recording(app, secs) => {
                let time = format!("{:02}:{:02}", secs / 60, secs % 60);
                (time.clone(), format!("{time}  {app}"), "Stop", None, accent())
            }
            PillState::Pending(app) => (
                String::new(),
                format!("Call  {app}"),
                "Record",
                Some("Ignore"),
                NSColor::systemOrangeColor(),
            ),
        };
        let recording = matches!(state, PillState::Recording(..));
        // A recording starts compact, even under the mouse that clicked Record.
        if recording && self.primary.action() != Some(sel!(stop:)) {
            self.armed.set(false);
            self.set_compact(true);
        }
        if !recording {
            self.set_compact(false);
        }
        self.dot.setTextColor(Some(&color));
        self.primary.setTitle(&NSString::from_str(primary));
        // Record stands out in the app's green; Stop stays plain.
        self.primary.setBezelColor((!recording).then(accent).as_deref());
        let action = if recording { sel!(stop:) } else { sel!(record:) };
        unsafe { self.primary.setAction(Some(action)) };
        self.secondary.setHidden(secondary.is_none());
        self.secondary.setTitle(&NSString::from_str(secondary.unwrap_or("")));
        *self.text.borrow_mut() = (short, full);
        self.set_label();
        if !self.panel.isVisible() {
            // Where it was left (its center, as it may have been compact then), if that screen is
            // still there; otherwise top center of the main screen.
            let saved = self.panel.setFrameUsingName(ns_string!("pill"));
            let f = self.panel.frame();
            let mid = NSPoint::new(f.origin.x + f.size.width / 2.0, f.origin.y + PILL_H / 2.0);
            let on_screen = NSScreen::screens(mtm).iter().any(|s| {
                let v = s.visibleFrame();
                mid.x >= v.origin.x
                    && mid.x <= v.origin.x + v.size.width
                    && mid.y >= v.origin.y
                    && mid.y <= v.origin.y + v.size.height
            });
            if saved && on_screen {
                self.panel.setFrame_display(
                    NSRect::new(
                        NSPoint::new(mid.x - PILL_W / 2.0, f.origin.y),
                        NSSize::new(PILL_W, PILL_H),
                    ),
                    false,
                );
            } else if let Some(screen) = NSScreen::mainScreen(mtm) {
                let f = screen.visibleFrame();
                let x = f.origin.x + (f.size.width - PILL_W) / 2.0;
                let y = f.origin.y + f.size.height - PILL_H - 10.0;
                self.panel
                    .setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(PILL_W, PILL_H)), false);
            }
            self.compact.set(false);
            self.set_compact(recording);
            self.panel.orderFrontRegardless();
        }
    }
}
