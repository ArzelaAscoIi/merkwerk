//! Screen activity as text: follows the frontmost window and reads its content through the
//! macOS Accessibility API. No screenshots, no OCR; password fields are never read.

use objc2_app_kit::NSWorkspace;
use objc2_application_services::{AXCopyMultipleAttributeOptions, AXError, AXUIElement};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType, CFURL};
use std::{collections::HashSet, ptr::NonNull};

pub struct Snapshot {
    pub app: String,
    pub bundle: Option<String>,
    pub window: String,
    pub url: Option<String>,
    pub text: String,
}

/// Apps and windows that are never captured (name, bundle id or window title fragments).
pub const DEFAULT_IGNORE: &str = "merkwerk,1password,bitwarden,lastpass,dashlane,keepass,keychain,loginwindow,screensaver,\
private browsing,incognito,inprivate";

const MAX_NODES: usize = 4000;
const MAX_CHARS: usize = 30_000;

/// Whether this process may use the Accessibility API; `prompt` shows the system dialog once.
pub fn trusted(prompt: bool) -> bool {
    let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
    let options = CFDictionary::from_slices(&[&*key], &[&**CFBoolean::new(prompt)]);
    unsafe { objc2_application_services::AXIsProcessTrustedWithOptions(Some(options.as_opaque())) }
}

/// Seconds since the last keyboard or mouse input.
pub fn idle_secs() -> f64 {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    // combined session state, any input event
    unsafe { CGEventSourceSecondsSinceLastEventType(0, u32::MAX) }
}

/// The frontmost app's name, without touching Accessibility (cheap).
pub fn frontmost_app() -> Option<String> {
    Some(
        NSWorkspace::sharedWorkspace()
            .frontmostApplication()?
            .localizedName()?
            .to_string(),
    )
}

/// What the frontmost window shows right now, or None when nothing readable is in front.
/// `prepared` remembers which Chromium/Electron apps already got their accessibility tree enabled.
pub fn snapshot(prepared: &mut HashSet<i32>) -> Option<Snapshot> {
    let front = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let app = front.localizedName().map(|n| n.to_string()).unwrap_or_default();
    let bundle = front.bundleIdentifier().map(|b| b.to_string());
    snapshot_app(front.processIdentifier(), app, bundle, prepared)
}

fn snapshot_app(pid: i32, app: String, bundle: Option<String>, prepared: &mut HashSet<i32>) -> Option<Snapshot> {
    let ax = unsafe { AXUIElement::new_application(pid) };
    unsafe { ax.set_messaging_timeout(0.5) };
    if prepared.insert(pid) {
        // Chromium and Electron only build their accessibility tree when asked to.
        let _ = unsafe {
            ax.set_attribute_value(
                &CFString::from_static_str("AXManualAccessibility"),
                CFBoolean::new(true),
            )
        };
    }
    let window = element(&ax, "AXFocusedWindow")?;
    let mut walk = Walk {
        text: String::new(),
        url: None,
        nodes: 0,
        attrs: CFArray::from_retained_objects(&WALK_ATTRS.map(CFString::from_static_str)),
    };
    walk.visit(&window, 0);
    Some(Snapshot {
        app,
        bundle,
        window: string(&window, "AXTitle").unwrap_or_default(),
        url: walk.url,
        text: redact(&walk.text),
    })
}

/// Case-insensitive fragment match against app name, bundle id and window title.
pub fn ignored(s: &Snapshot, ignore: &[String]) -> bool {
    let hay = format!("{} {} {}", s.app, s.bundle.as_deref().unwrap_or(""), s.window).to_lowercase();
    ignore.iter().any(|i| hay.contains(i.as_str()))
}

struct Walk {
    text: String,
    url: Option<String>,
    nodes: usize,
    /// WALK_ATTRS as a CFArray, built once per walk.
    attrs: CFRetained<CFArray<CFString>>,
}

/// Fetched in one cross-process call per element: role, value, title, children.
const WALK_ATTRS: [&str; 4] = ["AXRole", "AXValue", "AXTitle", "AXChildren"];

impl Walk {
    fn visit(&mut self, el: &AXUIElement, depth: usize) {
        if depth > 60 || self.nodes >= MAX_NODES || self.text.len() >= MAX_CHARS {
            return;
        }
        self.nodes += 1;
        let Some([role, value, title, kids]) = self.fetch(el) else {
            return;
        };
        let as_string = |v: Option<CFRetained<CFType>>| v?.downcast::<CFString>().ok().map(|s| s.to_string());
        let role = as_string(role).unwrap_or_default();
        match role.as_str() {
            "AXSecureTextField" | "AXMenuBar" | "AXMenu" => return,
            "AXStaticText" | "AXTextArea" | "AXTextField" | "AXHeading" => {
                if let Some(t) = as_string(value).or_else(|| as_string(title)) {
                    self.push(&t);
                }
            }
            // Browsers built on web tech (Vivaldi, Arc) render their own UI as a web area too; the
            // page is the one with an http(s) URL.
            "AXWebArea" if !self.url.as_deref().is_some_and(is_page) => {
                let url = attr(el, "AXURL")
                    .and_then(|u| u.downcast::<CFURL>().ok())
                    .map(|u| u.string().to_string());
                if url.as_deref().is_some_and(is_page) || self.url.is_none() {
                    self.url = url;
                }
            }
            _ => {}
        }
        let Some(kids) = kids.and_then(|k| k.downcast::<CFArray>().ok()) else {
            return;
        };
        // AXChildren is documented as an array of AXUIElements.
        let kids = unsafe { CFRetained::cast_unchecked::<CFArray<AXUIElement>>(kids) };
        for child in kids.iter() {
            self.visit(&child, depth + 1);
        }
    }

    /// WALK_ATTRS of one element; attributes it lacks come back as None.
    fn fetch(&self, el: &AXUIElement) -> Option<[Option<CFRetained<CFType>>; 4]> {
        let mut out: *const CFArray = std::ptr::null();
        let err = unsafe {
            el.copy_multiple_attribute_values(
                self.attrs.as_ref(),
                AXCopyMultipleAttributeOptions::empty(),
                NonNull::from(&mut out),
            )
        };
        let out = unsafe { CFRetained::from_raw(NonNull::new(out.cast_mut())?) };
        if err != AXError::Success {
            return None;
        }
        // One CFType per requested attribute; a missing one is an AXValue error marker, which
        // the CFString / CFArray downcasts in `visit` reject.
        let values = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(out) };
        Some([values.get(0), values.get(1), values.get(2), values.get(3)])
    }

    fn push(&mut self, t: &str) {
        let t = t.trim();
        if t.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(t);
    }
}

fn is_page(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn attr(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    let mut value: *const CFType = std::ptr::null();
    let err = unsafe { el.copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value)) };
    let value = NonNull::new(value.cast_mut())?;
    // Copy* returns a +1 reference; take ownership even on error so nothing leaks.
    let value = unsafe { CFRetained::from_raw(value) };
    (err == AXError::Success).then_some(value)
}

fn string(el: &AXUIElement, name: &str) -> Option<String> {
    attr(el, name)?.downcast::<CFString>().ok().map(|s| s.to_string())
}

fn element(el: &AXUIElement, name: &str) -> Option<CFRetained<AXUIElement>> {
    attr(el, name)?.downcast::<AXUIElement>().ok()
}

#[cfg(test)]
fn children(el: &AXUIElement) -> Vec<CFRetained<AXUIElement>> {
    let Some(array) = attr(el, "AXChildren").and_then(|a| a.downcast::<CFArray>().ok()) else {
        return vec![];
    };
    // AXChildren is documented as an array of AXUIElements.
    let array = unsafe { CFRetained::cast_unchecked::<CFArray<AXUIElement>>(array) };
    array.iter().collect()
}

/// Masks token-like words (API keys, access tokens): 24+ chars of [A-Za-z0-9_-] mixing letters
/// and digits.
// Limitation: shape heuristic; misses short secrets, may mask long hashes. Add patterns as needed.
fn redact(text: &str) -> String {
    let secret = |w: &str| {
        w.len() >= 24
            && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && w.chars().any(|c| c.is_ascii_digit())
            && w.chars().any(|c| c.is_ascii_alphabetic())
    };
    text.lines()
        .map(|line| {
            line.split(' ')
                .map(|w| if secret(w) { "[redacted]" } else { w })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_keeps_words() {
        let out = redact("key ghp_1234567890abcdefghijklmnopqrstuv here\nnormal words stay");
        assert_eq!(out, "key [redacted] here\nnormal words stay");
        assert_eq!(redact("2026-10-17_131739"), "2026-10-17_131739");
    }

    #[test]
    fn ignore_matches_title_fragments() {
        let s = Snapshot {
            app: "Vivaldi".into(),
            bundle: None,
            window: "Docs - Private Browsing".into(),
            url: None,
            text: String::new(),
        };
        let ignore: Vec<String> = DEFAULT_IGNORE.split(',').map(String::from).collect();
        assert!(ignored(&s, &ignore));
    }
}

#[cfg(test)]
mod live {
    #[test]
    #[ignore = "reads the real screen"]
    fn print_frontmost() {
        eprintln!("trusted: {}", super::trusted(false));
        eprintln!("idle: {:.1}s", super::idle_secs());
        let t = std::time::Instant::now();
        let pid = std::env::var("PID").ok().and_then(|p| p.parse().ok());
        let mut prepared = Default::default();
        let mut snap = None;
        for _ in 0..4 {
            snap = match pid {
                Some(pid) => super::snapshot_app(pid, "pid".into(), None, &mut prepared),
                None => super::snapshot(&mut prepared),
            };
            if snap.as_ref().is_some_and(|s| !s.text.is_empty()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        match snap {
            Some(s) => eprintln!(
                "app={} bundle={:?} window={:?} url={:?} chars={} took={:?}\n---\n{}",
                s.app,
                s.bundle,
                s.window,
                s.url,
                s.text.len(),
                t.elapsed(),
                &s.text.chars().take(600).collect::<String>()
            ),
            None => eprintln!("no snapshot ({:?})", t.elapsed()),
        }
    }
}

#[cfg(test)]
mod debug {
    use super::*;
    #[test]
    #[ignore = "reads the real screen"]
    fn roles() {
        let pid: i32 = std::env::var("PID").unwrap().parse().unwrap();
        let ax = unsafe { AXUIElement::new_application(pid) };
        let _ = unsafe {
            ax.set_attribute_value(
                &CFString::from_static_str("AXManualAccessibility"),
                CFBoolean::new(true),
            )
        };
        std::thread::sleep(std::time::Duration::from_secs(2));
        let windows = attr(&ax, "AXWindows").and_then(|a| a.downcast::<CFArray>().ok());
        eprintln!("windows: {:?}", windows.as_ref().map(|w| w.len()));
        let w = element(&ax, "AXFocusedWindow").or_else(|| element(&ax, "AXMainWindow"));
        let Some(w) = w else {
            eprintln!("no window");
            return;
        };
        let mut hist = std::collections::BTreeMap::<String, usize>::new();
        fn go(el: &AXUIElement, d: usize, h: &mut std::collections::BTreeMap<String, usize>, n: &mut usize) {
            if d > 80 || *n > 20000 {
                return;
            }
            *n += 1;
            *h.entry(string(el, "AXRole").unwrap_or_default()).or_default() += 1;
            for c in children(el) {
                go(&c, d + 1, h, n);
            }
        }
        let mut n = 0;
        go(&w, 0, &mut hist, &mut n);
        eprintln!("title={:?} nodes={n} roles={hist:?}", string(&w, "AXTitle"));
    }
}
