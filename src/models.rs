//! Background model downloads.

use crate::{config::Config, state::AppState, transcribe};
use std::sync::Arc;

/// Download the selected engine's missing model files in the background, one download at a time;
/// the menu shows "(downloading...)" meanwhile.
pub fn download_in_background(cfg: Config, state: Arc<AppState>) {
    let engine = cfg.engine();
    {
        let mut downloading = state.downloading.lock();
        if downloading.is_some() {
            return;
        }
        *downloading = Some(engine);
    }
    std::thread::spawn(move || {
        // Loop: the engine may be switched mid-download; a pick that found `downloading` set relies
        // on this thread. The installed check and clearing share the lock so no pick slips between.
        loop {
            {
                let mut downloading = state.downloading.lock();
                let engine = cfg.engine();
                if transcribe::installed(&cfg, &engine) {
                    *downloading = None;
                    return;
                }
                *downloading = Some(engine);
            }
            if let Err(e) = transcribe::ensure_models(&cfg, &state) {
                eprintln!("model download: {e:#}");
                state.downloading.set(None);
                return;
            }
        }
    });
}
