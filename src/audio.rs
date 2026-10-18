//! Capture: microphone ("me") and system output via CoreAudio loopback ("them").
//! Both arrive as mono f32 at the device rate and are resampled to 16 kHz for whisper.

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::Sender;

pub const RATE: u32 = 16_000;

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
    pub streams: Vec<cpal::Stream>,
    pub resamplers: [Resampler; 2],
}

pub fn start(tx: Sender<(Source, Vec<f32>)>) -> Result<Capture> {
    let host = cpal::default_host();
    let mic = host.default_input_device().context("no input device")?;
    let out = host.default_output_device().context("no output device")?;
    let (mic_stream, mic_rate) = open(&mic, mic.default_input_config()?, Source::Me, tx.clone())?;
    // Building an input stream on an output device is cpal's loopback (macOS 14.6+).
    let (out_stream, out_rate) = open(&out, out.default_output_config()?, Source::Them, tx)?;
    Ok(Capture {
        streams: vec![mic_stream, out_stream],
        resamplers: [Resampler::new(mic_rate), Resampler::new(out_rate)],
    })
}

fn open(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    source: Source,
    tx: Sender<(Source, Vec<f32>)>,
) -> Result<(cpal::Stream, u32)> {
    // Limitation: CoreAudio hands out f32; add conversions if another backend is ever needed.
    if config.sample_format() != cpal::SampleFormat::F32 {
        bail!("unsupported sample format {:?}", config.sample_format());
    }
    let channels = config.channels() as usize;
    let rate = config.sample_rate();
    let stream = device.build_input_stream(
        config.into(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mono = data
                .chunks(channels)
                .map(|f| f.iter().sum::<f32>() / channels as f32)
                .collect();
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
