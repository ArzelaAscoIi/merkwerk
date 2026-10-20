//! One recording session: capture -> wav files + live chunked transcription -> notes.

use crate::{
    audio::{self, RATE, Source, to_i16},
    calendar,
    config::Config,
    store::{self, Meta, Store},
    summarize,
    transcribe::{CHUNK_SECS, Transcriber},
};
use anyhow::{Context, Result, bail};
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
            let n = cut(&self.pending[i]);
            self.send(source, n);
        }
        Ok(())
    }

    /// Hand the first `n` pending samples to the transcriber.
    fn send(&mut self, source: Source, n: usize) {
        let i = source as usize;
        let rest = self.pending[i].split_off(n);
        let samples = std::mem::replace(&mut self.pending[i], rest);
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
            self.send(source, self.pending[source as usize].len());
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
        let mut at = 0;
        while at < samples.len() {
            eprintln!("{} {}%", source.label(), at * 100 / samples.len());
            let rest = &samples[at..];
            let n = if rest.len() > CHUNK { cut(rest) } else { rest.len() };
            let segs = transcriber.run(source, at as f64 / RATE as f64, &rest[..n])?;
            store.append_segments(id, &segs)?;
            at += n;
        }
    }
    Ok(())
}

/// Where to end a chunk of `samples` (at least CHUNK long): the middle of the quietest 100 ms in
/// its last 5 s, so the cut falls into a pause instead of splitting a word.
fn cut(samples: &[f32]) -> usize {
    const WIN: usize = RATE as usize / 10;
    let energy = |i: usize| samples[i..i + WIN].iter().map(|s| s * s).sum::<f32>();
    (CHUNK - 5 * RATE as usize..=CHUNK - WIN)
        .step_by(WIN / 2)
        .min_by(|&a, &b| energy(a).total_cmp(&energy(b)))
        .map_or(CHUNK, |i| i + WIN / 2)
}

/// Write transcript.md and summary.md. Untitled meetings take the calendar event's title, else
/// the summary's; the event's invitees are kept in meta.json and given to the notes step.
pub fn finalize(cfg: &Config, store: &Store, id: &str) -> Result<()> {
    // Nothing was said (a misclick, or a call that never got going): don't keep it.
    if store.transcript(id).trim().is_empty() {
        eprintln!("{id}: empty transcript, deleted");
        return std::fs::remove_dir_all(store.dir(id)).context("deleting empty recording");
    }
    let mut meta = store.load(id)?;
    let secs = |t: &str| {
        chrono::DateTime::parse_from_rfc3339(t)
            .ok()
            .map(|t| t.timestamp() as f64)
    };
    if meta.attendees.is_empty()
        && let Some(start) = secs(&meta.started_at)
        && let Some(event) = calendar::event_at(start, meta.ended_at.as_deref().and_then(secs).unwrap_or(start))
    {
        if meta.title == store::UNTITLED && !event.title.trim().is_empty() {
            meta.title = event.title;
        }
        meta.attendees = event.attendees;
        store.save(&meta)?;
    }
    // Rendered after the calendar lookup: a 1:1's invitee names the `them` lines.
    let transcript = store.transcript(id);
    store.write(id, "transcript.md", &transcript)?;
    let summary = summarize::summarize(cfg, &meta, &transcript)?;
    store.write(id, "summary.md", &summary)?;
    if meta.title == store::UNTITLED
        && let Some(title) = summarize::title(&summary)
    {
        meta.title = title.to_string();
        store.save(&meta)?;
    }
    eprintln!("{id}: done ({})", meta.title);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_lands_in_the_pause_near_the_end() {
        let mut samples = vec![0.5f32; CHUNK + RATE as usize];
        let pause = CHUNK - 2 * RATE as usize; // 200 ms of silence, 2 s before the 30 s mark
        samples[pause..pause + RATE as usize / 5].fill(0.0);
        let n = cut(&samples);
        assert!(
            (pause..pause + RATE as usize / 5).contains(&n),
            "cut at {n}, pause at {pause}"
        );
        assert!(n <= CHUNK);
    }
}
