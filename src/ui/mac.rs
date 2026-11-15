//! macOS menu bar indicator: `mw` while watching, `● 03:12` while recording, `◌` while writing notes.
//! The same status shows as a Dock badge (`REC`), since a full menu bar hides items behind the notch;
//! the Dock icon reopens the notes window and its right-click menu mirrors the menu bar menu.
//! The menu starts/stops a recording by hand (in-person talks) and lists recent meetings;
//! clicking one opens its folder.

use super::*;
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
    NSFontWeightMedium, NSImage, NSLineBreakMode, NSMenu, NSMenuItem, NSPanel, NSRequestUserAttentionType, NSScreen,
    NSStatusBar, NSStatusItem, NSStatusWindowLevel, NSTextAlignment, NSTextField, NSTitlePosition,
    NSVariableStatusItemLength, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString, NSTimer, ns_string};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView, WKWebViewConfiguration,
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    sync::atomic::Ordering,
    time::Instant,
};

/// The open notes window and the fingerprint of the data it shows.
type NotesWindow = (Retained<NSWindow>, Retained<WKWebView>, u64);

thread_local! {
    /// Folder behind each clickable menu item, indexed by the item's tag.
    static PATHS: RefCell<Vec<PathBuf>> = const { RefCell::new(vec![]) };
    /// Model choice behind each model menu item, indexed by the item's tag.
    static PICKS: RefCell<Vec<Pick>> = const { RefCell::new(vec![]) };
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
                super::open(path);
            }
        }

        #[unsafe(method(notes:))]
        fn notes(&self, _item: &NSMenuItem) {
            open_notes(MainThreadMarker::from(self));
        }

        #[unsafe(method(dictation:))]
        fn dictation(&self, _item: &NSMenuItem) {
            open_notes(MainThreadMarker::from(self));
            state().show_dictation.set(true);
        }

        #[unsafe(method(pick:))]
        fn pick(&self, item: &NSMenuItem) {
            let pick = PICKS.with_borrow(|p| p.get(item.tag() as usize).cloned());
            if let Some(pick) = pick {
                super::pick(pick);
            }
        }

        #[unsafe(method(ignore:))]
        fn ignore(&self, _sender: &NSObject) {
            state().ignore.set(true);
        }

        #[unsafe(method(screen:))]
        fn screen(&self, _sender: &NSObject) {
            toggle_screen();
        }

        #[unsafe(method(auto:))]
        fn auto(&self, _sender: &NSObject) {
            set_auto(!state().auto_record.is_set());
        }

        /// The pill's Always: turns auto-record on, which also records the call it is shown for.
        #[unsafe(method(always:))]
        fn always(&self, _sender: &NSObject) {
            set_auto(true);
        }

        /// The pill's Stop: only ever stops, so a late second click can't start a new recording.
        #[unsafe(method(stop:))]
        fn stop(&self, _sender: &NSObject) {
            state().stop.set(true);
        }

        /// The pill's Record: only ever starts, so a second click can't stop what the first started.
        #[unsafe(method(record:))]
        fn record(&self, _sender: &NSObject) {
            state().start.set(true);
        }

        #[unsafe(method(toggle:))]
        fn toggle(&self, _sender: &NSObject) {
            toggle_recording();
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
            handle_message(&raw);
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

/// Run the menu bar on the main thread. Never returns.
pub fn run(cfg: Config, store: Store, shared: Arc<AppState>) -> ! {
    let mtm = MainThreadMarker::new().expect("ui must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate: Retained<AppDelegate> = unsafe { msg_send![mtm.alloc::<AppDelegate>(), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    app.setMainMenu(Some(&main_menu(mtm)));
    set_icon(mtm, &app);
    set_store(&cfg, &store, &shared);
    watch_models(&cfg);
    open_notes(mtm);

    let handler: Retained<Handler> = unsafe { msg_send![mtm.alloc::<Handler>(), init] };
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let pill = std::rc::Rc::new(Pill::new(mtm, &handler));
    let meter = pill.clone();
    let hud = Hud::new(mtm);
    let level = block2::RcBlock::new(move |_: std::ptr::NonNull<NSTimer>| {
        // Before the pill: it resets the mic level it reads.
        hud.update(mtm);
        meter.level();
    });
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
    let data = NSData::with_bytes(include_bytes!("../../assets/logo.png"));
    if let Some(image) = NSImage::initWithData(mtm.alloc(), &data) {
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

fn set_title(item: &NSStatusItem, mtm: MainThreadMarker, title: &str) {
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str(title));
    }
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
            Row::Never(app) => {
                let item = add(
                    &format!("Never record {}", notes::app_name(app)),
                    Some(sel!(pick:)),
                    ns_string!(""),
                );
                unsafe { item.setTarget(Some(handler)) };
                item.setTag(picks.len() as isize);
                picks.push(Pick::Never(app.clone()));
            }
            Row::Auto(on) => {
                let item = add("Auto-record calls", Some(sel!(auto:)), ns_string!(""));
                unsafe { item.setTarget(Some(handler)) };
                item.setState(if *on {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
            }
            Row::Dictation(label) => unsafe {
                add(label, Some(sel!(dictation:)), ns_string!("")).setTarget(Some(handler))
            },
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
pub fn run_notes(cfg: Config, store: Store, shared: Arc<AppState>) -> ! {
    let mtm = MainThreadMarker::new().expect("ui must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.setMainMenu(Some(&main_menu(mtm)));
    set_icon(mtm, &app);
    set_store(&cfg, &store, &shared);
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
    let Some((cfg, store)) = store() else {
        return;
    };
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
        let (html, fingerprint) = notes_page(&cfg, &store);
        unsafe { web.loadHTMLString_baseURL(&NSString::from_str(&html), None) };
        *shown = fingerprint;
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
        // The context menu's Reload blanks a page loaded from an HTML string: load it again.
        if unsafe { !web.isLoading() && web.title().is_none_or(|t| t.length() == 0) } {
            let (html, fingerprint) = notes_page(cfg, store);
            unsafe { web.loadHTMLString_baseURL(&NSString::from_str(&html), None) };
            *shown = fingerprint;
            return;
        }
        for js in notes_updates(cfg, store, shown) {
            unsafe { web.evaluateJavaScript_completionHandler(&NSString::from_str(&js), None) };
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

/// Floating pill at the top of the screen, on every desktop: the recording indicator and the
/// Record/Ignore prompt for a detected call. Unlike the menu bar item, the notch can't hide it.
/// While recording it shrinks to a breathing dot and the time; hovering it shows the app and Stop.
struct Pill {
    panel: Retained<NSPanel>,
    dot: Retained<NSTextField>,
    label: Retained<NSTextField>,
    primary: Retained<NSButton>,
    secondary: Retained<NSButton>,
    always: Retained<NSButton>,
    /// The latest transcript lines, shown under the row while hovering a recording.
    said: Retained<NSView>,
    /// One (speaker, text) pair of labels per transcript line, oldest first.
    lines: Vec<(Retained<NSTextField>, Retained<NSTextField>)>,
    compact: Cell<bool>,
    /// Hover expands only once the mouse has left the pill since recording began, so the click on
    /// Record can't turn into a click on Stop.
    armed: Cell<bool>,
    /// Label text for the compact and the full pill.
    text: RefCell<(String, String)>,
    born: Instant,
}

/// Dock badge: `REC` while recording, `…` while writing notes, nothing otherwise.
fn badge() -> String {
    if state().recording.lock().is_some() {
        "REC".into()
    } else if !state().finishing.lock().is_empty() {
        "…".into()
    } else {
        String::new()
    }
}

/// The last two transcribed lines of the meeting being recorded, as (speaker, text), for the
/// hovered pill. Calls are transcribed in 30 s chunks, so this trails the conversation by up to that.
fn latest_said() -> Vec<(String, String)> {
    let Some((_, store)) = store() else {
        return vec![];
    };
    let Some(m) = store.list().into_iter().find(|m| store.live(m)) else {
        return vec![];
    };
    let them = store.them_name(&m.id).unwrap_or_else(|| "Them".into());
    let segments = store.segments(&m.id);
    if segments.is_empty() {
        return vec![(String::new(), "Transcript starts after the first 30 seconds".into())];
    }
    segments[segments.len().saturating_sub(SAID_LINES)..]
        .iter()
        .map(|s| {
            let who = if s.source == crate::audio::Source::Me {
                "Me".into()
            } else {
                them.clone()
            };
            (who, s.text.clone())
        })
        .collect()
}

/// Transcript lines under the hovered recording pill.
const SAID_LINES: usize = 2;
const PILL_W: f64 = 380.0;
const PILL_H: f64 = 28.0;
const PILL_COMPACT_W: f64 = 76.0;
/// Extra height of the hovered recording pill for the latest transcript lines.
const SAID_H: f64 = 44.0;

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
            // The row stays at the top when the pill grows for the transcript.
            t.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
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
            b.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
            );
            root.addSubview(&b);
            b
        };
        // Record is the big one: it is what a detected call is shown for.
        let primary = button(sel!(record:), PILL_W - 100.0, 96.0, NSControlSize::Regular);
        let secondary = button(sel!(ignore:), PILL_W - 160.0, 56.0, NSControlSize::Small);
        let always = button(sel!(always:), PILL_W - 220.0, 56.0, NSControlSize::Small);
        always.setTitle(ns_string!("Always"));
        always.setToolTip(Some(ns_string!("Record this call and every call from now on")));
        // Caption lines under the row: speaker in grey, then one line of text that keeps the
        // newest words visible.
        let said = NSView::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PILL_W, SAID_H)),
        );
        let lines = (0..SAID_LINES)
            .map(|i| {
                let y = SAID_H - 8.0 - 17.0 * (i + 1) as f64;
                let who = NSTextField::labelWithString(ns_string!(""), mtm);
                who.setFrame(NSRect::new(NSPoint::new(16.0, y), NSSize::new(56.0, 15.0)));
                who.setTextColor(Some(&NSColor::colorWithWhite_alpha(0.55, 1.0)));
                who.setFont(Some(&NSFont::systemFontOfSize_weight(11.0, unsafe {
                    NSFontWeightMedium
                })));
                who.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                let text = NSTextField::labelWithString(ns_string!(""), mtm);
                text.setFrame(NSRect::new(NSPoint::new(76.0, y), NSSize::new(PILL_W - 92.0, 15.0)));
                text.setTextColor(Some(&NSColor::colorWithWhite_alpha(0.92, 1.0)));
                text.setFont(Some(&NSFont::systemFontOfSize(12.0)));
                text.setLineBreakMode(NSLineBreakMode::ByTruncatingHead);
                said.addSubview(&who);
                said.addSubview(&text);
                (who, text)
            })
            .collect();
        said.setHidden(true);
        root.addSubview(&said);
        panel.setContentView(Some(&root));
        Pill {
            panel,
            dot,
            label,
            primary,
            secondary,
            always,
            said,
            lines,
            compact: Cell::new(false),
            armed: Cell::new(false),
            text: RefCell::default(),
            born: Instant::now(),
        }
    }

    /// The dot breathes slowly and lights up with what the mic or system audio hears. Also expands
    /// the recording pill while the mouse is over it.
    fn level(&self) {
        let peak = state().take_audio_level();
        let recording = state().recording.lock().is_some();
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
            250.0
        } else {
            130.0
        };
        // The row sits on top; the captions, when shown, take the space below it.
        let y = if self.said.isHidden() { 6.0 } else { SAID_H + 6.0 };
        self.label
            .setFrame(NSRect::new(NSPoint::new(26.0, y), NSSize::new(width, 16.0)));
    }

    fn show_said(&self) {
        let said = latest_said();
        for (i, (who, text)) in self.lines.iter().enumerate() {
            // Fewer lines than rows: fill from the bottom, so the newest is always last.
            let line = (i + said.len()).checked_sub(SAID_LINES).and_then(|k| said.get(k));
            let (w, t) = line.map_or(("", ""), |(w, t)| (w.as_str(), t.as_str()));
            who.setStringValue(&NSString::from_str(w));
            text.setStringValue(&NSString::from_str(t));
        }
    }

    /// Resizes around the same center; the compact pill shows only the dot and the time.
    fn set_compact(&self, compact: bool) {
        // Hovering a recording also shows what was said last, below the row.
        let tall = !compact && state().recording.lock().is_some();
        if compact == self.compact.get() && tall != self.said.isHidden() {
            return;
        }
        self.compact.set(compact);
        self.primary.setHidden(compact);
        if tall {
            self.show_said();
        }
        self.said.setHidden(!tall);
        let width = if compact { PILL_COMPACT_W } else { PILL_W };
        let height = if tall { PILL_H + SAID_H } else { PILL_H };
        let f = self.panel.frame();
        let x = f.origin.x + (f.size.width - width) / 2.0;
        let top = f.origin.y + f.size.height;
        self.panel.setFrame_display(
            NSRect::new(NSPoint::new(x, top - height), NSSize::new(width, height)),
            true,
        );
        // After the resize: the row's labels move with the top edge, so place them in the final size.
        self.set_label();
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
        self.always.setHidden(secondary.is_none());
        self.secondary.setTitle(&NSString::from_str(secondary.unwrap_or("")));
        *self.text.borrow_mut() = (short, full);
        self.set_label();
        if recording && !self.said.isHidden() {
            self.show_said();
        }
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
            self.said.setHidden(true);
            self.set_compact(recording);
            self.panel.orderFrontRegardless();
        }
    }
}

const HUD_W: f64 = 460.0;
const HUD_H: f64 = 44.0;

/// The dictation overlay at the bottom of the screen: Listening (with the mic level), Transcribing,
/// then the dictated words for a few seconds.
struct Hud {
    panel: Retained<NSPanel>,
    dot: Retained<NSTextField>,
    label: Retained<NSTextField>,
    shown: RefCell<Option<crate::dictate::Overlay>>,
}

impl Hud {
    fn new(mtm: MainThreadMarker) -> Self {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(HUD_W, HUD_H));
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
        panel.setIgnoresMouseEvents(true);
        panel.setOpaque(false);
        panel.setHasShadow(true);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        let root = NSView::initWithFrame(mtm.alloc(), rect);
        let background = NSBox::initWithFrame(mtm.alloc(), rect);
        background.setBoxType(NSBoxType::Custom);
        background.setTitlePosition(NSTitlePosition::NoTitle);
        background.setBorderWidth(0.0);
        background.setCornerRadius(14.0);
        background.setFillColor(&NSColor::colorWithWhite_alpha(0.09, 0.94));
        root.addSubview(&background);
        let dot = NSTextField::labelWithString(ns_string!("●"), mtm);
        dot.setFrame(NSRect::new(
            NSPoint::new(12.0, (HUD_H - 16.0) / 2.0),
            NSSize::new(16.0, 16.0),
        ));
        dot.setTextColor(Some(&NSColor::systemRedColor()));
        root.addSubview(&dot);
        let label = NSTextField::wrappingLabelWithString(ns_string!(""), mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(34.0, 5.0),
            NSSize::new(HUD_W - 48.0, HUD_H - 10.0),
        ));
        label.setTextColor(Some(&NSColor::colorWithWhite_alpha(0.95, 1.0)));
        label.setFont(Some(&NSFont::systemFontOfSize_weight(13.0, unsafe {
            NSFontWeightMedium
        })));
        label.setMaximumNumberOfLines(2);
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingHead);
        root.addSubview(&label);
        panel.setContentView(Some(&root));
        Hud {
            panel,
            dot,
            label,
            shown: RefCell::new(None),
        }
    }

    /// Called every 100 ms.
    fn update(&self, mtm: MainThreadMarker) {
        use crate::dictate::Overlay;
        let mut now = state().overlay.lock().clone();
        if let Overlay::Said(_, until) = &now
            && Instant::now() >= *until
        {
            now = Overlay::Hidden;
            *state().overlay.lock() = Overlay::Hidden;
        }
        if now == Overlay::Listening {
            // The dot follows the mic level, like the recording pill.
            let peak = f32::from_bits(state().audio_level.load(Ordering::Relaxed));
            self.dot.setAlphaValue((0.35 + 1.5 * peak.sqrt()).min(1.0) as f64);
        }
        if self.shown.borrow().as_ref() == Some(&now) {
            return;
        }
        let (text, color) = match &now {
            Overlay::Hidden => {
                self.panel.orderOut(None);
                *self.shown.borrow_mut() = Some(now);
                return;
            }
            Overlay::Listening => ("Listening… release to paste".to_string(), NSColor::systemRedColor()),
            Overlay::Transcribing => ("Transcribing…".to_string(), NSColor::systemOrangeColor()),
            Overlay::Said(text, _) => (text.clone(), accent()),
        };
        self.dot.setTextColor(Some(&color));
        self.dot.setAlphaValue(1.0);
        self.label.setStringValue(&NSString::from_str(&text));
        if !self.panel.isVisible()
            && let Some(screen) = NSScreen::mainScreen(mtm)
        {
            let f = screen.visibleFrame();
            let x = f.origin.x + (f.size.width - HUD_W) / 2.0;
            self.panel.setFrame_display(
                NSRect::new(NSPoint::new(x, f.origin.y + 60.0), NSSize::new(HUD_W, HUD_H)),
                false,
            );
        }
        self.panel.orderFrontRegardless();
        *self.shown.borrow_mut() = Some(now);
    }
}
