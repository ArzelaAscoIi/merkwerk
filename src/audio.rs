//! Capture: microphone ("me") and system output ("them"): CoreAudio / WASAPI loopback through cpal,
//! on Linux the PulseAudio / PipeWire monitor of the default output through `parec`.
//! Both arrive as mono f32 at the device rate and are resampled to 16 kHz for whisper.

use anyhow::{Context, Result, bail};
use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::Sender;

pub const RATE: u32 = 16_000;

/// Loudest sample since the UI last looked, as f32 bits (non-negative floats order like their bits).
pub static LEVEL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Me,
    Them,
}

impl Source {
    pub const ALL: [Source; 2] = [Source::Me, Source::Them];

    pub fn label(self) -> &'static str {
        match self {
            Source::Me => "me",
            Source::Them => "them",
        }
    }
}

/// Live capture. Dropping it stops both streams.
pub struct Capture {
    /// cpal streams, and on Linux the `parec` process; dropping one stops it.
    pub streams: Vec<Box<dyn std::any::Any>>,
    pub resamplers: [Resampler; 2],
}

pub fn start(tx: Sender<(Source, Vec<f32>)>) -> Result<Capture> {
    let (mic_stream, mic_rate) = open_mic(tx.clone())?;
    let (out_stream, out_rate) = system_audio(tx)?;
    Ok(Capture {
        streams: vec![Box::new(mic_stream), out_stream],
        resamplers: [Resampler::new(mic_rate), Resampler::new(out_rate)],
    })
}

/// Building an input stream on an output device is cpal's loopback (macOS 14.6+, WASAPI).
#[cfg(not(target_os = "linux"))]
fn system_audio(tx: Sender<(Source, Vec<f32>)>) -> Result<(Box<dyn std::any::Any>, u32)> {
    let out = cpal::default_host()
        .default_output_device()
        .context("no output device")?;
    let (stream, rate) = open(&out, out.default_output_config()?, Source::Them, tx)?;
    Ok((Box::new(stream), rate))
}

/// ALSA has no loopback; record the default output's monitor through PulseAudio / pipewire-pulse.
#[cfg(target_os = "linux")]
fn system_audio(tx: Sender<(Source, Vec<f32>)>) -> Result<(Box<dyn std::any::Any>, u32)> {
    use std::io::Read;
    let mut child = std::process::Command::new("parec")
        .args([
            "--device=@DEFAULT_MONITOR@",
            "--format=float32le",
            "--channels=1",
            "--latency-msec=100",
        ])
        .arg(format!("--rate={RATE}"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("parec (pulseaudio-utils) is needed to record the other side of calls")?;
    let mut out = child.stdout.take().context("parec stdout")?;
    std::thread::spawn(move || {
        let mut buf = vec![0u8; RATE as usize / 10 * 4];
        while out.read_exact(&mut buf).is_ok() {
            let block: Vec<f32> = buf.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            let peak = block.iter().fold(0f32, |m, x| m.max(x.abs()));
            LEVEL.fetch_max(peak.to_bits(), std::sync::atomic::Ordering::Relaxed);
            if tx.send((Source::Them, block)).is_err() {
                break;
            }
        }
    });
    struct Parec(std::process::Child);
    impl Drop for Parec {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    Ok((Box::new(Parec(child)), RATE))
}

/// The microphone alone (dictation), with its resampler to 16 kHz.
pub fn mic(tx: Sender<(Source, Vec<f32>)>) -> Result<(cpal::Stream, Resampler)> {
    let (stream, rate) = open_mic(tx)?;
    Ok((stream, Resampler::new(rate)))
}

fn open_mic(tx: Sender<(Source, Vec<f32>)>) -> Result<(cpal::Stream, u32)> {
    let mic = cpal::default_host().default_input_device().context("no input device")?;
    open(&mic, mic.default_input_config()?, Source::Me, tx)
}

fn open(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    source: Source,
    tx: Sender<(Source, Vec<f32>)>,
) -> Result<(cpal::Stream, u32)> {
    use cpal::SampleFormat as F;
    match config.sample_format() {
        F::F32 => open_as::<f32>(device, config, source, tx),
        F::I16 => open_as::<i16>(device, config, source, tx),
        F::I32 => open_as::<i32>(device, config, source, tx),
        F::U16 => open_as::<u16>(device, config, source, tx),
        f => bail!("unsupported sample format {f:?}"),
    }
}

fn open_as<T: SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    source: Source,
    tx: Sender<(Source, Vec<f32>)>,
) -> Result<(cpal::Stream, u32)>
where
    f32: FromSample<T>,
{
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    let stream = device.build_input_stream(
        config.into(),
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mono: Vec<f32> = data
                .chunks(channels)
                .map(|f| f.iter().map(|&x| <f32 as FromSample<T>>::from_sample_(x)).sum::<f32>() / channels as f32)
                .collect();
            let peak = mono.iter().fold(0f32, |m: f32, x: &f32| m.max(x.abs()));
            LEVEL.fetch_max(peak.to_bits(), std::sync::atomic::Ordering::Relaxed);
            let _ = tx.send((source, mono));
        },
        move |e| eprintln!("audio ({}): {e}", source.label()),
        None,
    )?;
    stream.play()?;
    Ok((stream, rate))
}

/// Streaming linear resampler to 16 kHz.
// Limitation: linear interpolation, no anti-alias filter; fine for speech-to-text, swap for a sinc resampler for hi-fi.
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from: u32) -> Self {
        Resampler {
            step: from as f64 / RATE as f64,
            pos: 0.0,
            prev: 0.0,
        }
    }

    /// `pos` is relative to the current block; position -1 is the last sample of the previous block.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let last = (input.len() - 1) as f64;
        while self.pos < last {
            let i = self.pos.floor();
            let a = if i < 0.0 { self.prev } else { input[i as usize] };
            let b = input[(i + 1.0) as usize];
            out.push(a + (b - a) * (self.pos - i) as f32);
            self.pos += self.step;
        }
        self.pos -= input.len() as f64;
        self.prev = input[input.len() - 1];
    }
}

pub fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_48k_ramp_across_blocks() {
        let input: Vec<f32> = (0..4800).map(|i| i as f32).collect();
        let mut r = Resampler::new(48_000);
        let mut out = vec![];
        for block in input.chunks(333) {
            r.process(block, &mut out);
        }
        assert!((1599..=1600).contains(&out.len()), "len {}", out.len());
        for (k, v) in out.iter().enumerate() {
            assert!((v - (k * 3) as f32).abs() < 1e-3, "out[{k}] = {v}");
        }
    }
}
