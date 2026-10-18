//! One recording session: capture -> wav files + live chunked transcription -> notes.

use crate::{
    audio::{self, RATE, Source, to_i16},
    config::Config,
    store::{self, Meta, Store},
    summarize,
    transcribe::{CHUNK_SECS, Transcriber},
};
use anyhow::{Result, bail};
use hound::{WavReader, WavSpec, WavWriter};
use std::{
    fs::{self, File},
    io::BufWriter,
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CHUNK: usize = CHUNK_SECS * RATE as usize;
const WAV: WavSpec = WavSpec {
    channels: 1,
    sample_rate: RATE,
    bits_per_sample: 16,
    sample_format: hound::SampleFormat::Int,
};

type Chunk = (Source, f64, Vec<f32>);

/// A finished capture whose transcription may still be catching up.
pub struct Recording {
    pub meta: Meta,
    worker: JoinHandle<()>,
}

impl Recording {
    /// Wait for the last chunks to be transcribed, then write transcript.md and summary.md.
    pub fn finish(self, cfg: &Config, store: &Store) -> Result<()> {
        let _ = self.worker.join();
        // The speech model (several hundred MB) was just dropped; hand the freed pages back to
        // macOS instead of letting the allocator keep them for the rest of the day.
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) }; // all zones, as much as possible
        finalize(cfg, store, &self.meta.id)
    }
}

/// Record until `should_stop` (polled every 2s) returns true.
pub fn record(
    cfg: &Config,
    store: &Store,
    title: &str,
    app: Option<String>,
    mut should_stop: impl FnMut() -> bool,
) -> Result<Recording> {
    let mut transcriber = Transcriber::load(cfg)?;
    let (tx, rx) = mpsc::channel();
    let mut capture = audio::start(tx)?;
    let mut meta = store.create(title, app)?;
    eprintln!("recording -> {}", store.dir(&meta.id).display());

    let (chunk_tx, chunk_rx) = mpsc::channel::<Chunk>();
    let worker = {
        let (store, id) = (store.clone(), meta.id.clone());
        thread::spawn(move || {
            for (source, offset, samples) in chunk_rx {
                let res = transcriber
                    .run(source, offset, &samples)
                    .and_then(|s| store.append_segments(&id, &s));
                if let Err(e) = res {
                    eprintln!("transcribe: {e:#}");
                }
            }
        })
    };

    let mut sink = Sink::new(store, &meta.id, chunk_tx)?;
    let mut last_check = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok((source, raw)) => sink.push(&mut capture.resamplers[source as usize], source, &raw)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("audio streams stopped"),
        }
        if last_check.elapsed() >= Duration::from_secs(2) {
            last_check = Instant::now();
            // Keeps the wav headers valid if the process gets killed mid-call.
            sink.flush()?;
            if should_stop() {
                break;
            }
        }
    }
    let mut resamplers = capture.resamplers;
    drop(capture.streams);
    for (source, raw) in rx.try_iter() {
        sink.push(&mut resamplers[source as usize], source, &raw)?;
    }
    sink.finish()?;

    meta.ended_at = Some(chrono::Local::now().to_rfc3339());
    store.save(&meta)?;
    Ok(Recording { meta, worker })
}

/// Writes 16 kHz audio to the wav files and cuts it into transcription chunks.
struct Sink {
    wavs: [WavWriter<BufWriter<File>>; 2],
    pending: [Vec<f32>; 2],
    /// Samples already handed to the transcriber, per source; gives each chunk its time offset.
    sent: [usize; 2],
    chunk_tx: Sender<Chunk>,
}

impl Sink {
    fn new(store: &Store, id: &str, chunk_tx: Sender<Chunk>) -> Result<Self> {
        Ok(Sink {
            wavs: [
                WavWriter::create(store.wav_path(id, Source::Me), WAV)?,
                WavWriter::create(store.wav_path(id, Source::Them), WAV)?,
            ],
            pending: Default::default(),
            sent: [0; 2],
            chunk_tx,
        })
    }

    fn push(&mut self, resampler: &mut audio::Resampler, source: Source, raw: &[f32]) -> Result<()> {
        let i = source as usize;
        let from = self.pending[i].len();
        resampler.process(raw, &mut self.pending[i]);
        for &s in &self.pending[i][from..] {
            self.wavs[i].write_sample(to_i16(s))?;
        }
        if self.pending[i].len() >= CHUNK {
            self.send(source);
        }
        Ok(())
    }

    fn send(&mut self, source: Source) {
        let i = source as usize;
        let samples = std::mem::take(&mut self.pending[i]);
        let offset = self.sent[i] as f64 / RATE as f64;
        self.sent[i] += samples.len();
        let _ = self.chunk_tx.send((source, offset, samples));
    }

    fn flush(&mut self) -> Result<()> {
        for w in &mut self.wavs {
            w.flush()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        for source in Source::ALL {
            self.send(source);
        }
        for w in self.wavs {
            w.finalize()?;
        }
        Ok(())
    }
}

/// Re-run whisper over the saved wav files (after a crash, or with a better model).
pub fn retranscribe(cfg: &Config, store: &Store, id: &str) -> Result<()> {
    let mut transcriber = Transcriber::load(cfg)?;
    let _ = fs::remove_file(store.dir(id).join("transcript.jsonl"));
    for source in Source::ALL {
        let path = store.wav_path(id, source);
        let Ok(reader) = WavReader::open(&path) else { continue };
        let samples: Vec<f32> = reader
            .into_samples::<i16>()
            .filter_map(Result::ok)
            .map(|s| s as f32 / i16::MAX as f32)
            .collect();
        for (n, chunk) in samples.chunks(CHUNK).enumerate() {
            eprintln!("{} {}/{}", source.label(), n + 1, samples.len().div_ceil(CHUNK));
            let segs = transcriber.run(source, (n * CHUNK_SECS) as f64, chunk)?;
            store.append_segments(id, &segs)?;
        }
    }
    Ok(())
}

/// Write transcript.md and summary.md; untitled meetings take the title from the summary.
pub fn finalize(cfg: &Config, store: &Store, id: &str) -> Result<()> {
    let transcript = store.transcript(id);
    store.write(id, "transcript.md", &transcript)?;
    if transcript.trim().is_empty() {
        eprintln!("{id}: empty transcript, no summary");
        return Ok(());
    }
    let summary = summarize::summarize(cfg, &transcript)?;
    store.write(id, "summary.md", &summary)?;
    let mut meta = store.load(id)?;
    if meta.title == store::UNTITLED
        && let Some(title) = summarize::title(&summary)
    {
        meta.title = title.to_string();
        store.save(&meta)?;
    }
    eprintln!("{id}: done ({})", meta.title);
    Ok(())
}
