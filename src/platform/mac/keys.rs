//! Dictation keys and pasting on macOS: modifier state from CoreGraphics, paste with ⌘V.

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;
use std::{ffi::c_void, thread, time::Duration};
pub const UNAVAILABLE: Option<&str> = None;
pub const ONLY_MODIFIERS: &str = "Use only fn, ⌃, ⌥, ⇧ and ⌘: any other key would also be typed into the app.";

/// Keys a dictation shortcut may use, in display order: macOS virtual key code and label.
/// Modifiers only: holding them types nothing, so any other key press or click during the hold
/// means a shortcut (⌘C, ⌥-letter, ⌘-click) and cancels the dictation.
// ponytail: a letter or Space in the shortcut would also be typed into the app; allowing those
// needs a CGEventTap that swallows them.
pub const MODIFIERS: [(u16, &str); 9] = [
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

/// Names for `MERKWERK_DICTATE` and settings from before shortcuts were recorded, e.g. "right_command"
/// or "control+option".
pub fn named(name: &str) -> Option<u16> {
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

pub fn down(key: u16) -> bool {
    unsafe { CGEventSourceKeyState(HID, key) }
}

/// Key downs and mouse clicks so far; a change during a hold means it was a shortcut or ⌘-click.
pub fn presses() -> u32 {
    unsafe { CGEventSourceCounterForEventType(HID, 10) + CGEventSourceCounterForEventType(HID, 1) }
}

/// Pastes through the clipboard (⌘V), then puts the previous text back. False without Accessibility.
// ponytail: restores plain text only; an image on the clipboard is lost. Key code 9 is V on
// QWERTY/QWERTZ; Dvorak would need the layout lookup.
pub fn paste(text: &str) -> bool {
    if !super::screen::trusted(false) {
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
