//! Dictation keys and pasting: Windows reads modifier state with GetAsyncKeyState and pastes with
//! Ctrl+V; Linux has no portable key state (Wayland forbids it), so dictation is unavailable there.

/// Windows: modifier state through GetAsyncKeyState, paste with Ctrl+V.
#[cfg(windows)]
mod imp {
    use std::{thread, time::Duration};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

    pub const UNAVAILABLE: Option<&str> = None;
    pub const ONLY_MODIFIERS: &str =
        "Use only Ctrl, Alt, Shift, Win and F13–F24: any other key would also be typed into the app.";

    /// Virtual key codes and labels, in display order. F13–F24 type nothing; remappers send them.
    pub const KEYS: [(u16, &str); 20] = [
        (0xA2, "Ctrl"),
        (0xA3, "Right Ctrl"),
        (0xA4, "Alt"),
        (0xA5, "Right Alt"),
        (0xA0, "Shift"),
        (0xA1, "Right Shift"),
        (0x5B, "Win"),
        (0x5C, "Right Win"),
        (0x7C, "F13"),
        (0x7D, "F14"),
        (0x7E, "F15"),
        (0x7F, "F16"),
        (0x80, "F17"),
        (0x81, "F18"),
        (0x82, "F19"),
        (0x83, "F20"),
        (0x84, "F21"),
        (0x85, "F22"),
        (0x86, "F23"),
        (0x87, "F24"),
    ];

    pub fn named(name: &str) -> Option<u16> {
        Some(match name.trim() {
            "control" | "ctrl" => 0xA2,
            "right_control" => 0xA3,
            "option" | "alt" => 0xA4,
            "right_option" | "right_alt" => 0xA5,
            "shift" => 0xA0,
            "right_shift" => 0xA1,
            "command" | "cmd" | "win" => 0x5B,
            "right_command" | "right_win" => 0x5C,
            f => return KEYS.iter().find(|k| k.1.eq_ignore_ascii_case(f)).map(|k| k.0),
        })
    }

    pub fn down(key: u16) -> bool {
        unsafe { GetAsyncKeyState(key as i32) as u16 & 0x8000 != 0 }
    }

    /// Other keys and mouse buttons held right now, leaving out the shortcut's own keys; a change
    /// during a hold means a shortcut.
    // ponytail: a key tapped between two 10 ms polls goes unseen; a low-level keyboard hook if it matters.
    pub fn presses(shortcut: &[u16]) -> u32 {
        let modifier = |vk: u16| matches!(vk, 0x10..=0x12 | 0x5B | 0x5C | 0xA0..=0xA5);
        (1u16..0xFF)
            .filter(|vk| !modifier(*vk) && !shortcut.contains(vk) && down(*vk))
            .map(u32::from)
            .sum()
    }

    /// Pastes through the clipboard (Ctrl+V), then puts the previous text back.
    pub fn paste(text: &str) -> bool {
        let Ok(mut clip) = arboard::Clipboard::new() else {
            return false;
        };
        let old = clip.get_text().ok();
        if clip.set_text(text).is_err() {
            return false;
        }
        let key = |vk: u16, up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [
            key(VK_CONTROL, false),
            key(0x56, false),
            key(0x56, true),
            key(VK_CONTROL, true),
        ];
        unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
        // The target app reads the clipboard asynchronously after Ctrl+V.
        thread::sleep(Duration::from_millis(300));
        if let Some(old) = old {
            let _ = clip.set_text(old);
        }
        true
    }
}

/// Linux: no portable way to read global key state (Wayland forbids it).
// ponytail: X11 could poll XQueryKeymap; add when someone on X11 asks.
#[cfg(target_os = "linux")]
mod imp {
    pub const UNAVAILABLE: Option<&str> = Some("Dictation isn't available on Linux yet.");
    pub const ONLY_MODIFIERS: &str = "";
    pub const KEYS: [(u16, &str); 0] = [];

    pub fn named(_name: &str) -> Option<u16> {
        None
    }

    pub fn down(_key: u16) -> bool {
        false
    }

    pub fn presses(_shortcut: &[u16]) -> u32 {
        0
    }

    pub fn paste(_text: &str) -> bool {
        false
    }
}

pub use imp::*;
