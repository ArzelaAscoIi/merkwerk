//! Everything that differs per operating system lives here, behind the same function names.
//! Callers use `crate::platform::<module>::...` and never write `#[cfg(target_os = ...)]`
//! themselves; each OS has one directory (`mac`, `other` for Windows and Linux) with the same
//! modules. `unsafe` belongs in this directory too.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(not(target_os = "macos"))]
mod other;

#[cfg(target_os = "macos")]
pub use mac::{bundle, calendar, detect, keys, screen, system};
#[cfg(not(target_os = "macos"))]
pub use other::{calendar, detect, keys, screen, system};
