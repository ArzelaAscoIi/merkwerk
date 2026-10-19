//! Local speech-to-text: Parakeet TDT 0.6B v3 (ONNX, default) or whisper.cpp (Metal).
//! Silero VAD picks the speech first in both cases.

use crate::{
    audio::Source,
    config::{Config, MODEL_URL, MODEL_V2_URL, PARAKEET_FILES, PARAKEET_URL, VAD_URL},
    store::Segment,
};
use anyhow::{Context, Result, bail};
use parakeet_rs::ort::environment::Environment;
use parakeet_rs::{ExecutionConfig, ParakeetTDT, TimestampMode, Transcriber as _};
use std::{fs, io, path::Path, sync::OnceLock};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext, WhisperVadContextParams,
    WhisperVadParams,
};

/// ONNX Runtime allows one environment per process; every recording after the first reuses it.
static ORT_ENV: OnceLock<Environment> = OnceLock::new();

fn ort_env() -> Result<Environment> {
    if let Some(env) = ORT_ENV.get() {
        return Ok(env.clone());
    }
    let env = parakeet_rs::ort::init().build()?;
    Ok(ORT_ENV.get_or_init(|| env).clone())
}

/// Audio is transcribed in chunks of at most this many seconds (whisper's native window).
pub const CHUNK_SECS: usize = 30;

pub struct Transcriber {
    engine: Engine,
    vad: WhisperVadContext,
}

enum Engine {
    /// Detects the language itself (25 European languages) and predicts punctuation.
    Parakeet(Box<ParakeetTDT>),
    Whisper {
        ctx: WhisperContext,
        /// Candidate languages; with several, each utterance gets the most likely one of them.
        langs: Vec<String>,
    },
}

impl Transcriber {
    pub fn load(cfg: &Config) -> Result<Self> {
        for path in model_files(cfg, &cfg.engine()).iter().map(|(p, _)| p).chain([&cfg.vad]) {
            if !path.exists() {
                bail!("no model at {}, run `merkwerk model`", path.display());
            }
        }
        whisper_rs::install_logging_hooks(); // silences whisper.cpp/ggml stderr chatter
        let engine = match cfg.engine().as_str() {
            "parakeet" => Engine::Parakeet(Box::new(
                ParakeetTDT::from_pretrained(&cfg.parakeet, Some(ExecutionConfig::new().with_environment(ort_env()?)))
                    .with_context(|| format!("loading Parakeet from {}", cfg.parakeet.display()))?,
            )),
            engine @ ("whisper" | "whisper-v2") => {
                let model = if engine == "whisper" { &cfg.model } else { &cfg.model_v2 };
                Engine::Whisper {
                    ctx: WhisperContext::new_with_params(model, WhisperContextParameters::default())
                        .with_context(|| format!("loading whisper model {}", model.display()))?,
                    langs: cfg.lang.split(',').map(|l| l.trim().to_string()).collect(),
                }
            }
            other => bail!("unknown MERKWERK_ENGINE {other:?}, use parakeet, whisper or whisper-v2"),
        };
        let vad = WhisperVadContext::new(&cfg.vad.to_string_lossy(), WhisperVadContextParams::default())
            .with_context(|| format!("loading VAD model {}", cfg.vad.display()))?;
        Ok(Transcriber { engine, vad })
    }

    /// Transcribe 16 kHz mono `samples` that start `offset` seconds into the meeting.
    /// Speech models hallucinate ("Thank you.") on silence and noise, so only the stretches Silero
    /// VAD marks as speech are transcribed. (whisper.cpp's own `vad` flag is ignored by the
    /// `whisper_full_with_state` path whisper-rs uses, hence doing it here.)
    pub fn run(&mut self, source: Source, offset: f64, samples: &[f32]) -> Result<Vec<Segment>> {
        let mut vad_params = WhisperVadParams::new();
        vad_params.set_min_silence_duration(800); // ms; merges pauses within a sentence
        vad_params.set_speech_pad(200); // ms
        let speech: Vec<_> = self.vad.segments_from_samples(vad_params, samples)?.collect();
        let mut out = vec![];
        for region in speech {
            // VAD timestamps are centiseconds.
            let from = (region.start as usize * 160).min(samples.len());
            let to = (region.end as usize * 160).min(samples.len());
            if to > from {
                let at = offset + from as f64 / 16_000.0;
                let region = &samples[from..to];
                out.extend(match &mut self.engine {
                    Engine::Parakeet(model) => parakeet(model, source, at, region)?,
                    Engine::Whisper { ctx, langs } => whisper(ctx, langs, source, at, region)?,
                });
            }
        }
        Ok(out)
    }
}

fn parakeet(model: &mut ParakeetTDT, source: Source, offset: f64, samples: &[f32]) -> Result<Vec<Segment>> {
    let result = model.transcribe_samples(samples.to_vec(), 16_000, 1, Some(TimestampMode::Sentences))?;
    Ok(result
        .tokens
        .into_iter()
        .map(|t| (t.start, t.end, t.text.trim().to_string()))
        .filter(|(_, _, text)| !text.is_empty())
        .map(|(start, end, text)| Segment {
            source,
            start: offset + start as f64,
            end: offset + end as f64,
            text,
        })
        .collect())
}

fn whisper(
    ctx: &WhisperContext,
    langs: &[String],
    source: Source,
    offset: f64,
    samples: &[f32],
) -> Result<Vec<Segment>> {
    let mut state = ctx.create_state()?;
    let lang = pick_lang(langs, &mut state, samples)?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some(lang));
    params.set_no_context(true);
    params.set_suppress_blank(true);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);

    state.full(params, samples)?;
    let mut out = vec![];
    for i in 0..state.full_n_segments() {
        let Some(seg) = state.get_segment(i) else { continue };
        let text = seg.to_str_lossy()?.trim().to_string();
        if text.is_empty() || is_annotation(&text) || seg.no_speech_probability() > 0.6 {
            continue;
        }
        out.push(Segment {
            source,
            start: offset + seg.start_timestamp() as f64 / 100.0,
            end: offset + seg.end_timestamp() as f64 / 100.0,
            text,
        });
    }
    Ok(out)
}

/// Restricting detection to the languages actually spoken (e.g. en,de) keeps short
/// utterances like "ja" or "okay" from being detected as some third language.
fn pick_lang<'a>(langs: &'a [String], state: &mut whisper_rs::WhisperState, samples: &[f32]) -> Result<&'a str> {
    if langs.len() == 1 {
        return Ok(&langs[0]);
    }
    state.pcm_to_mel(samples, 4)?;
    let (_, probs) = state.lang_detect(0, 4)?;
    let prob = |l: &String| {
        whisper_rs::get_lang_id(l)
            .and_then(|id| probs.get(id as usize).copied())
            .unwrap_or(0.0)
    };
    Ok(langs
        .iter()
        .max_by(|a, b| prob(a).total_cmp(&prob(b)))
        .context("no language")?)
}

/// `[BLANK_AUDIO]`, `(music)` and friends.
fn is_annotation(t: &str) -> bool {
    (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')'))
}

/// A speech engine's model files, with their download URLs.
fn model_files(cfg: &Config, engine: &str) -> Vec<(std::path::PathBuf, String)> {
    match engine {
        "whisper" => vec![(cfg.model.clone(), MODEL_URL.into())],
        "whisper-v2" => vec![(cfg.model_v2.clone(), MODEL_V2_URL.into())],
        _ => PARAKEET_FILES
            .iter()
            .map(|f| (cfg.parakeet.join(f), format!("{PARAKEET_URL}/{f}")))
            .collect(),
    }
}

/// Download the speech and VAD models if missing.
pub fn ensure_models(cfg: &Config) -> Result<()> {
    for (path, url) in model_files(cfg, &cfg.engine()) {
        ensure_model(&path, &url)?;
    }
    ensure_model(&cfg.vad, VAD_URL)
}

/// Download one model file if it is missing.
fn ensure_model(path: &Path, url: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("model path has no parent")?)?;
    eprintln!("downloading {} ...", path.display());
    let tmp = path.with_extension("part");
    let mut resp = ureq::get(url).call()?;
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    io::copy(&mut reader, &mut fs::File::create(&tmp)?)?;
    fs::rename(&tmp, path)?;
    eprintln!("model ready");
    Ok(())
}

/// Whether an engine's model files are already downloaded.
pub fn installed(cfg: &Config, engine: &str) -> bool {
    model_files(cfg, engine).iter().all(|(p, _)| p.exists())
}

#[cfg(test)]
mod reload {
    /// A second recording in the same process used to fail: "only one environment is allowed per process".
    #[test]
    #[ignore = "needs the downloaded models"]
    fn load_twice() {
        let cfg = crate::config::Config::from_env();
        super::Transcriber::load(&cfg).unwrap();
        super::Transcriber::load(&cfg).unwrap();
    }
}
