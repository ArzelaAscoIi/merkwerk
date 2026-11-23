//! Windows and Linux: a tray icon with the status menu, the notes page in a webview window, and the
//! recording pill and dictation overlay as small always-on-top webview windows.
// ponytail: the pill is a plain dark bar (no hover-to-expand, no live captions, no saved position)
// and the overlay shows only its text; port the macOS details when someone misses them.

use super::*;
use std::time::{Duration, Instant};
use tao::{
    dpi::{LogicalPosition, LogicalSize},
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
    window::{Icon as WindowIcon, Window, WindowBuilder},
};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};
use wry::{WebView, WebViewBuilder};

enum UserEvent {
    /// The pill was grabbed: move it with the mouse.
    Drag,
}

/// What a tray menu item does; its menu id is its index in the current list.
enum Action {
    Toggle,
    Ignore,
    Screen,
    Notes,
    Dictation,
    Open(PathBuf),
    Pick(Pick),
    Quit,
}

/// The notes page bridge: the page talks to `window.webkit.messageHandlers.merkwerk` like on macOS.
const BRIDGE: &str =
    "window.webkit = { messageHandlers: { merkwerk: { postMessage: m => window.ipc.postMessage(m) } } };";

const PILL_W: f64 = 380.0;
const PILL_H: f64 = 34.0;
const HUD_W: f64 = 460.0;
const HUD_H: f64 = 48.0;

const PILL_HTML: &str = r#"<!doctype html><html><body style="margin:0;height:100vh;display:flex;align-items:center;
gap:8px;padding:0 10px;box-sizing:border-box;background:#171717;color:#f2f2f2;font:500 12px system-ui,sans-serif;
user-select:none;cursor:default;overflow:hidden">
<span id="dot" style="color:#3cc497">●</span><span id="label" style="flex:1;white-space:nowrap;overflow:hidden;
text-overflow:ellipsis"></span><span id="buttons"></span>
<script>
document.body.onmousedown = e => { if (e.target.tagName !== "BUTTON") ipc.postMessage("drag"); };
function set(s) {
  label.textContent = s.label;
  dot.style.color = s.color;
  buttons.innerHTML = "";
  for (const [text, action, main] of s.buttons) {
    const b = document.createElement("button");
    b.textContent = text;
    b.style.cssText = "margin-left:6px;border:0;border-radius:6px;padding:4px 10px;font:inherit;cursor:pointer;"
      + (main ? "background:#3cc497;color:#0b0b0b" : "background:#333;color:#eee");
    b.onclick = () => ipc.postMessage(action);
    buttons.append(b);
  }
}
</script></body></html>"#;

const HUD_HTML: &str = r#"<!doctype html><html><body style="margin:0;height:100vh;display:flex;align-items:center;
gap:10px;padding:0 14px;box-sizing:border-box;background:#171717;color:#f2f2f2;font:500 13px system-ui,sans-serif;
user-select:none;overflow:hidden">
<span id="dot">●</span><span id="label" style="overflow:hidden;display:-webkit-box;-webkit-line-clamp:2;
-webkit-box-orient:vertical"></span>
<script>function set(text, color) { label.textContent = text; dot.style.color = color; }</script></body></html>"#;

/// assets/logo.png as RGBA.
fn logo() -> (Vec<u8>, u32, u32) {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&include_bytes!("../../assets/logo.png")[..]));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().expect("logo.png");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("logo.png");
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
    };
    (rgba, info.width, info.height)
}

fn webview(window: &Window, builder: WebViewBuilder) -> WebView {
    #[cfg(not(target_os = "linux"))]
    let web = builder.build(window);
    #[cfg(target_os = "linux")]
    let web = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().expect("tao gtk box"))
    };
    web.expect("webview")
}

/// A small always-on-top window without frame or taskbar entry.
fn floating(target: &EventLoopWindowTarget<UserEvent>, w: f64, h: f64) -> Window {
    let builder = WindowBuilder::new()
        .with_title("merkwerk")
        .with_decorations(false)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_focused(false)
        .with_visible(false)
        .with_inner_size(LogicalSize::new(w, h));
    #[cfg(windows)]
    let builder = tao::platform::windows::WindowBuilderExtWindows::with_skip_taskbar(builder, true);
    #[cfg(target_os = "linux")]
    let builder = tao::platform::unix::WindowBuilderExtUnix::with_skip_taskbar(builder, true);
    builder.build(target).expect("window")
}

/// Puts a floating window at the horizontal center of the primary screen, `y` from the top (or
/// from the bottom when negative).
fn place(window: &Window, w: f64, h: f64, y: f64) {
    let Some(screen) = window.primary_monitor() else { return };
    let size = screen.size().to_logical::<f64>(screen.scale_factor());
    let top = if y < 0.0 { size.height - h + y } else { y };
    window.set_outer_position(LogicalPosition::new((size.width - w) / 2.0, top));
}

struct Notes {
    window: Window,
    web: WebView,
    shown: u64,
}

/// Shows the notes window, creating it on first use; a fresh page on every open.
fn open_notes(target: &EventLoopWindowTarget<UserEvent>, slot: &mut Option<Notes>) {
    let Some((cfg, store)) = store() else { return };
    let (html, shown) = notes_page(&cfg, &store);
    match slot {
        Some(n) => {
            let _ = n.web.load_html(&html);
            n.shown = shown;
        }
        None => {
            let (rgba, w, h) = logo();
            let window = WindowBuilder::new()
                .with_title("merkwerk")
                .with_inner_size(LogicalSize::new(1180.0, 780.0))
                .with_min_inner_size(LogicalSize::new(640.0, 420.0))
                .with_window_icon(WindowIcon::from_rgba(rgba, w, h).ok())
                .build(target)
                .expect("notes window");
            let web = webview(
                &window,
                WebViewBuilder::new()
                    .with_initialization_script(BRIDGE)
                    .with_html(html)
                    .with_ipc_handler(|req| handle_message(req.body())),
            );
            *slot = Some(Notes { window, web, shown });
        }
    }
    if let Some(n) = slot {
        n.window.set_visible(true);
        n.window.set_focus();
    }
}

fn refresh_notes(slot: &mut Option<Notes>) {
    let (Some(n), Some((cfg, store))) = (slot.as_mut(), store()) else {
        return;
    };
    for js in notes_updates(&cfg, &store, &mut n.shown) {
        let _ = n.web.evaluate_script(&js);
    }
}

/// Tray menu from the shared rows; returns the menu and what each item id does.
fn menu(rows: &[Row]) -> (Menu, Vec<Action>) {
    let menu = Menu::new();
    let mut actions = vec![];
    let mut id = |a: Action| {
        actions.push(a);
        (actions.len() - 1).to_string()
    };
    let add = |i: &dyn IsMenuItem| {
        let _ = menu.append(i);
    };
    for row in rows {
        match row {
            Row::Separator => add(&PredefinedMenuItem::separator()),
            Row::Label(label) => add(&MenuItem::new(label, false, None)),
            Row::Toggle(label) => add(&MenuItem::with_id(id(Action::Toggle), label, true, None)),
            Row::Ignore => add(&MenuItem::with_id(id(Action::Ignore), "Ignore this call", true, None)),
            Row::Never(app) => add(&MenuItem::with_id(
                id(Action::Pick(Pick::Never(app.clone()))),
                format!("Never record {}", crate::notes::app_name(app)),
                true,
                None,
            )),
            Row::Notes => add(&MenuItem::with_id(id(Action::Notes), "Open notes", true, None)),
            Row::Dictation(label) => add(&MenuItem::with_id(id(Action::Dictation), label, true, None)),
            Row::Open(label, path) => add(&MenuItem::with_id(id(Action::Open(path.clone())), label, true, None)),
            Row::Screen(on) => add(&CheckMenuItem::with_id(
                id(Action::Screen),
                "Record screen activity",
                true,
                *on,
                None,
            )),
            Row::Choice(title, options) => {
                let sub = Submenu::new(*title, true);
                for (label, selected, pick) in options {
                    let _ = sub.append(&CheckMenuItem::with_id(
                        id(Action::Pick(pick.clone())),
                        label,
                        true,
                        *selected,
                        None,
                    ));
                }
                add(&sub);
            }
        }
    }
    add(&MenuItem::with_id(id(Action::Quit), "Quit", true, None));
    (menu, actions)
}

struct Pill {
    window: Window,
    web: WebView,
    shown: Option<String>,
}

impl Pill {
    fn new(target: &EventLoopWindowTarget<UserEvent>, proxy: EventLoopProxy<UserEvent>) -> Self {
        let window = floating(target, PILL_W, PILL_H);
        let web = webview(
            &window,
            WebViewBuilder::new()
                .with_html(PILL_HTML)
                .with_ipc_handler(move |req| match req.body().as_str() {
                    "drag" => drop(proxy.send_event(UserEvent::Drag)),
                    // Each button only ever does its one thing, so a late second click can't undo the first.
                    "record" => state().start.set(true),
                    "stop" => state().stop.set(true),
                    "ignore" => state().ignore.set(true),
                    _ => {}
                }),
        );
        place(&window, PILL_W, PILL_H, 10.0);
        Pill {
            window,
            web,
            shown: None,
        }
    }

    fn show(&mut self, state: &PillState) {
        let s = match state {
            PillState::Hidden => None,
            PillState::Recording(app, secs) => Some(serde_json::json!({
                "label": format!("{:02}:{:02}  {app}", secs / 60, secs % 60),
                "color": "#3cc497",
                "buttons": [["Stop", "stop", false]],
            })),
            PillState::Pending(app) => Some(serde_json::json!({
                "label": format!("Call  {app}"),
                "color": "#ff9f0a",
                "buttons": [["Ignore", "ignore", false], ["Record", "record", true]],
            })),
        }
        .map(|s| s.to_string());
        if s == self.shown {
            return;
        }
        match &s {
            Some(js) => {
                let _ = self.web.evaluate_script(&format!("set({js})"));
                if self.shown.is_none() {
                    self.window.set_visible(true);
                }
            }
            None => self.window.set_visible(false),
        }
        self.shown = s;
    }
}

/// The dictation overlay near the bottom of the screen.
struct Hud {
    window: Window,
    web: WebView,
    shown: Option<crate::dictate::Overlay>,
}

impl Hud {
    fn new(target: &EventLoopWindowTarget<UserEvent>) -> Self {
        let window = floating(target, HUD_W, HUD_H);
        let web = webview(&window, WebViewBuilder::new().with_html(HUD_HTML));
        place(&window, HUD_W, HUD_H, -60.0);
        Hud {
            window,
            web,
            shown: None,
        }
    }

    fn update(&mut self) {
        use crate::dictate::Overlay;
        let mut now = state().overlay.lock().clone();
        if let Overlay::Said(_, until) = &now
            && Instant::now() >= *until
        {
            now = Overlay::Hidden;
            *state().overlay.lock() = Overlay::Hidden;
        }
        if self.shown.as_ref() == Some(&now) {
            return;
        }
        let (text, color) = match &now {
            Overlay::Hidden => {
                self.window.set_visible(false);
                self.shown = Some(now);
                return;
            }
            Overlay::Listening => ("Listening… release to paste".to_string(), "#ff453a"),
            Overlay::Transcribing => ("Transcribing…".to_string(), "#ff9f0a"),
            Overlay::Said(text, _) => (text.clone(), "#3cc497"),
        };
        let _ = self.web.evaluate_script(&format!(
            "set({}, {})",
            serde_json::json!(text),
            serde_json::json!(color)
        ));
        self.window.set_visible(true);
        // Only once shown: on Linux the window has no GDK surface before that (tao panics).
        let _ = self.window.set_ignore_cursor_events(true);
        self.shown = Some(now);
    }
}

/// The tray app on the main thread. Never returns.
pub fn run(cfg: Config, store: Store, shared: Arc<AppState>) -> ! {
    set_store(&cfg, &store, &shared);
    watch_models(&cfg);
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let mut tray: Option<TrayIcon> = None;
    let mut actions: Vec<Action> = vec![];
    let mut notes: Option<Notes> = None;
    let mut pill: Option<Pill> = None;
    let mut hud: Option<Hud> = None;
    let (mut last_menu, mut last_title) = (String::new(), String::new());
    let mut ticks: u64 = 0;
    let mut next_second = Instant::now();
    event_loop.run(move |event, target, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));
        match event {
            Event::NewEvents(StartCause::Init) => {
                let (rgba, w, h) = logo();
                tray = TrayIconBuilder::new()
                    .with_icon(Icon::from_rgba(rgba, w, h).expect("tray icon"))
                    .with_tooltip("merkwerk")
                    .build()
                    .map_err(|e| eprintln!("tray icon: {e}"))
                    .ok();
                pill = Some(Pill::new(target, proxy.clone()));
                hud = Some(Hud::new(target));
                open_notes(target, &mut notes);
            }
            Event::UserEvent(UserEvent::Drag) => {
                if let Some(p) = &pill {
                    let _ = p.window.drag_window();
                }
            }
            // Closing the notes window drops it, so its webview frees its memory; the tray stays.
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } if notes.as_ref().is_some_and(|n| n.window.id() == window_id) => notes = None,
            _ => {}
        }
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                open_notes(target, &mut notes);
            }
        }
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            match e.id.0.parse::<usize>().ok().and_then(|i| actions.get(i)) {
                Some(Action::Toggle) => toggle_recording(),
                Some(Action::Ignore) => state().ignore.set(true),
                Some(Action::Screen) => toggle_screen(),
                Some(Action::Notes) => open_notes(target, &mut notes),
                Some(Action::Dictation) => {
                    open_notes(target, &mut notes);
                    state().show_dictation.set(true);
                }
                Some(Action::Open(path)) => open(path),
                Some(Action::Pick(p)) => pick(p.clone()),
                Some(Action::Quit) => std::process::exit(0),
                None => {}
            }
        }
        if let Some(h) = &mut hud {
            h.update();
        }
        if Instant::now() < next_second {
            return;
        }
        next_second = Instant::now() + Duration::from_secs(1);
        if let Some(p) = &mut pill {
            p.show(&pill_state());
        }
        refresh_notes(&mut notes);
        let Some(tray) = &tray else { return };
        let now = title();
        if now != last_title {
            let _ = tray.set_tooltip(Some(format!("merkwerk  {now}")));
            // Shown next to the icon on Linux (AppIndicator); Windows has only the tooltip.
            tray.set_title(Some(&now));
            last_title = now;
        }
        // The menu reads meeting files; every 5 s is plenty, and only rebuilt on change.
        ticks += 1;
        if ticks % 5 != 1 {
            return;
        }
        let rows = entries(&store);
        let key = format!("{rows:?}");
        if key != last_menu {
            let (m, a) = menu(&rows);
            tray.set_menu(Some(Box::new(m)));
            actions = a;
            last_menu = key;
        }
    })
}

/// Just the notes window (`merkwerk notes`); exits when it closes.
pub fn run_notes(cfg: Config, store: Store, shared: Arc<AppState>) -> ! {
    set_store(&cfg, &store, &shared);
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let mut notes: Option<Notes> = None;
    event_loop.run(move |event, target, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(1));
        match event {
            Event::NewEvents(StartCause::Init) => open_notes(target, &mut notes),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => std::process::exit(0),
            _ => refresh_notes(&mut notes),
        }
    })
}
