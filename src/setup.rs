//! First-run checks behind the "Get started" page: a 5 second test that both sides of a call are
//! heard, and downloading the notes model into Ollama.

use crate::{
    audio::{self, Source},
    config::Config,
};
use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use serde::Serialize;
use std::{
    io::BufRead,
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

/// Loudest sample that counts as heard: room noise stays below, speech and the tone get above.
const HEARD: f32 = 0.02;
const TEST: Duration = Duration::from_secs(5);

#[derive(Clone, Default, Serialize)]
pub struct Test {
    pub running: bool,
    /// None until a test finished.
    pub me: Option<bool>,
    pub them: Option<bool>,
    pub error: Option<String>,
}

pub static TEST_STATE: Mutex<Test> = Mutex::new(Test {
    running: false,
    me: None,
    them: None,
    error: None,
});

/// Records both sides for 5 s while playing a short tone; the user says something meanwhile.
/// On macOS this is also when the microphone and system audio permissions are asked for.
pub fn start_test() {
    {
        let mut t = TEST_STATE.lock().unwrap();
        if t.running {
            return;
        }
        *t = Test {
            running: true,
            ..Test::default()
        };
    }
    thread::spawn(|| {
        let result = run_test();
        *TEST_STATE.lock().unwrap() = match result {
            Ok((me, them)) => Test {
                running: false,
                me: Some(me),
                them: Some(them),
                error: None,
            },
            Err(e) => Test {
                running: false,
                error: Some(format!("{e:#}")),
                ..Test::default()
            },
        };
    });
}

pub fn run_test() -> anyhow::Result<(bool, bool)> {
    let (tx, rx) = mpsc::channel();
    let capture = audio::start(tx)?;
    // The tone starts a moment later, so loopback is already listening.
    thread::sleep(Duration::from_millis(500));
    let tone = play_tone().map_err(|e| eprintln!("test tone: {e:#}")).ok();
    let mut peak = [0f32; 2];
    let start = Instant::now();
    while start.elapsed() < TEST {
        if let Ok((source, block)) = rx.recv_timeout(Duration::from_millis(100)) {
            let p = block.iter().fold(0f32, |m, x| m.max(x.abs()));
            peak[source as usize] = peak[source as usize].max(p);
        }
    }
    drop(tone);
    drop(capture);
    Ok((peak[Source::Me as usize] > HEARD, peak[Source::Them as usize] > HEARD))
}

/// A soft 660 Hz tone for 1.5 s on the default output, which loopback records as "them".
fn play_tone() -> anyhow::Result<cpal::Stream> {
    use anyhow::Context;
    let device = cpal::default_host()
        .default_output_device()
        .context("no output device")?;
    let config = device.default_output_config()?;
    use cpal::SampleFormat as F;
    match config.sample_format() {
        F::F32 => tone::<f32>(&device, config),
        F::I16 => tone::<i16>(&device, config),
        F::I32 => tone::<i32>(&device, config),
        F::U16 => tone::<u16>(&device, config),
        f => anyhow::bail!("unsupported output format {f:?}"),
    }
}

fn tone<T: SizedSample + FromSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
) -> anyhow::Result<cpal::Stream> {
    let channels = config.channels() as usize;
    let rate = config.sample_rate() as f32;
    let length = (rate * 1.5) as u64;
    let mut n = 0u64;
    let stream = device.build_output_stream(
        config.into(),
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
            for frame in out.chunks_mut(channels) {
                let s = if n < length {
                    // Fade in and out over 50 ms so it doesn't click.
                    let fade = (n.min(length - n) as f32 / (rate * 0.05)).min(1.0);
                    0.2 * fade * (n as f32 * 660.0 * std::f32::consts::TAU / rate).sin()
                } else {
                    0.0
                };
                n += 1;
                frame.fill(T::from_sample(s));
            }
        },
        |e| eprintln!("test tone: {e}"),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// Where the user fixes a side that wasn't heard: a settings page to open, and what to do there.
pub fn fix(side: Source) -> (Option<&'static str>, &'static str) {
    #[cfg(target_os = "macos")]
    return match side {
        Source::Me => (
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"),
            "Allow merkwerk under Microphone, or pick another input in Sound settings.",
        ),
        Source::Them => (
            Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"),
            "Turn the volume up, and allow merkwerk under Screen & System Audio Recording. Start merkwerk.app, not the terminal.",
        ),
    };
    #[cfg(windows)]
    return match side {
        Source::Me => (
            Some("ms-settings:privacy-microphone"),
            "Allow desktop apps to use the microphone.",
        ),
        Source::Them => (
            Some("ms-settings:sound"),
            "Turn the volume up on your default speakers.",
        ),
    };
    #[cfg(target_os = "linux")]
    return match side {
        Source::Me => (None, "Check the input device in your sound settings."),
        Source::Them => (
            None,
            "Turn the volume up. merkwerk records the default output through parec (pulseaudio-utils).",
        ),
    };
}

/// The notes model being downloaded into Ollama: percent done, or why it failed.
#[derive(Clone, Serialize)]
pub struct Pull {
    pub percent: Option<u64>,
    pub error: Option<String>,
}

pub static PULL: Mutex<Option<Pull>> = Mutex::new(None);

/// `ollama pull <notes model>` through Ollama's API, in the background.
pub fn pull_model(cfg: &Config) {
    {
        let mut pull = PULL.lock().unwrap();
        if pull.as_ref().is_some_and(|p| p.error.is_none()) {
            return;
        }
        *pull = Some(Pull {
            percent: None,
            error: None,
        });
    }
    let (url, model) = (cfg.llm_url(), cfg.llm_model());
    thread::spawn(move || {
        let error = run_pull(&url, &model).err().map(|e| format!("{e:#}"));
        if let Some(e) = &error {
            eprintln!("ollama pull {model}: {e}");
        }
        // Done: clear it; the notes check sees the model on its next round.
        *PULL.lock().unwrap() = error.map(|error| Pull {
            percent: None,
            error: Some(error),
        });
    });
}

fn run_pull(chat_url: &str, model: &str) -> anyhow::Result<()> {
    let base = chat_url.split_once("/v1/").map_or(chat_url, |(b, _)| b);
    let mut resp = ureq::post(&format!("{base}/api/pull")).send_json(serde_json::json!({ "model": model }))?;
    let reader = std::io::BufReader::new(resp.body_mut().with_config().limit(u64::MAX).reader());
    // One JSON object per line: {"status", "total", "completed"} while downloading, or {"error"}.
    for line in reader.lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        if let Some(e) = v["error"].as_str() {
            anyhow::bail!("{e}");
        }
        if let (Some(done), Some(total)) = (v["completed"].as_u64(), v["total"].as_u64())
            && total > 0
        {
            *PULL.lock().unwrap() = Some(Pull {
                percent: Some(done * 100 / total),
                error: None,
            });
        }
    }
    Ok(())
}
