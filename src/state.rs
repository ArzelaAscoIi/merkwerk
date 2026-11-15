//! Runtime state shared between threads: one [`AppState`] per process, created in `main` and
//! handed to whoever needs it. No module owns a `static`; this is the only place that says what
//! the threads share.

use crate::{
    dictate::{Capture, Overlay},
    setup::{Pull, Test},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Instant,
};

/// A boolean one thread raises and another consumes.
#[derive(Debug, Default)]
pub struct Flag(AtomicBool);

impl Flag {
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::Relaxed);
    }

    /// Returns whether the flag was raised and lowers it.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

/// A mutex that survives a panicked holder: the data is plain state, never half-updated invariants.
#[derive(Debug, Default)]
pub struct Shared<T>(Mutex<T>);

impl<T> Shared<T> {
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set(&self, value: T) {
        *self.lock() = value;
    }
}

impl<T: Clone> Shared<T> {
    pub fn get(&self) -> T {
        self.lock().clone()
    }
}

/// What the threads (watch loop, screen activity, dictation, background jobs, UI) share.
#[derive(Default)]
pub struct AppState {
    // UI to watch loop: requests, consumed with `Flag::take`.
    pub start: Flag,
    pub stop: Flag,
    /// Ignore the detected call until its app releases the mic.
    pub ignore: Flag,

    // Settings mirrored for the hot loops; persisted in settings.json.
    pub auto_record: Flag,
    pub screen: Flag,

    // Watch loop to UI: what is going on.
    /// The app being recorded and when it started.
    pub recording: Shared<Option<(String, Instant)>>,
    /// A call detected in manual mode, waiting for Record or Ignore.
    pub pending: Shared<Option<String>>,
    /// Ids of meetings still being transcribed or summarized.
    pub finishing: Shared<Vec<String>>,
    /// Speech engine whose model is being downloaded.
    pub downloading: Shared<Option<String>>,
    /// Bytes downloaded and expected (0 when unknown) of the model file being downloaded.
    pub download_progress: Shared<Option<(u64, u64)>>,
    /// Loudest sample since the UI last looked, as f32 bits (non-negative floats order like their bits).
    pub audio_level: Arc<AtomicU32>,
    /// Optional process-wide PostHog client, initialized once at startup.
    pub posthog: Shared<Option<Arc<posthog_rs::Client>>>,

    // Dictation.
    /// The shortcut: these keys held together. Empty = off.
    pub shortcut: Shared<Vec<u16>>,
    pub capture: Shared<Capture>,
    pub overlay: Shared<Overlay>,

    // First-run setup.
    pub audio_test: Shared<Test>,
    pub model_pull: Shared<Option<Pull>>,

    // Notes window.
    /// Why notes can't be written right now, in plain words; None when fine or unknown.
    pub notes_problem: Shared<Option<String>>,
    /// The notes server answered its last model list request.
    pub notes_reachable: Flag,
    /// Models offered by the notes server, refreshed in the background.
    pub server_models: Shared<Vec<String>>,
    /// Chat answers waiting to be handed to the page: (request id, answer JSON).
    pub answers: Shared<Vec<(u64, String)>>,
    /// The page wants a full resend (a delete failed after the page dropped the row).
    pub resend: Flag,
    /// The menu asked for the Dictation tab.
    pub show_dictation: Flag,
    /// Each meeting's page JSON, keyed by a hash of what it was built from.
    pub meeting_cache: Shared<HashMap<String, (u64, Value)>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Loudest audio sample since the last call (0.0 when silent).
    pub fn take_audio_level(&self) -> f32 {
        f32::from_bits(self.audio_level.swap(0, Ordering::Relaxed))
    }

    /// Download progress of the current model file in whole percent, if known.
    pub fn download_percent(&self) -> Option<u64> {
        self.download_progress
            .lock()
            .filter(|(_, total)| *total > 0)
            .map(|(done, total)| done * 100 / total)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.lock().is_some()
    }
}
