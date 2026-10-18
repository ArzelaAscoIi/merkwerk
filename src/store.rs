//! Plain files, one directory per meeting:
//!   meta.json  me.wav  them.wav  transcript.jsonl  transcript.md  summary.md

use crate::audio::Source;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};

/// Placeholder title; replaced by the title from the generated notes.
pub const UNTITLED: &str = "Untitled";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meta {
    pub id: String,
    pub title: String,
    /// Process that triggered auto-recording; None for manual recordings.
    pub app: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Segment {
    pub source: Source,
    /// Seconds from meeting start.
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn create(&self, title: &str, app: Option<String>) -> Result<Meta> {
        let now = chrono::Local::now();
        let meta = Meta {
            id: now.format("%Y-%m-%d_%H%M%S").to_string(),
            title: title.to_string(),
            app,
            started_at: now.to_rfc3339(),
            ended_at: None,
        };
        fs::create_dir_all(self.dir(&meta.id))?;
        self.save(&meta)?;
        Ok(meta)
    }

    pub fn save(&self, meta: &Meta) -> Result<()> {
        fs::write(
            self.dir(&meta.id).join("meta.json"),
            serde_json::to_string_pretty(meta)?,
        )?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Meta> {
        let raw = fs::read_to_string(self.dir(id).join("meta.json")).with_context(|| format!("no meeting {id}"))?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// All meetings, newest first.
    pub fn list(&self) -> Vec<Meta> {
        let mut all: Vec<Meta> = fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| self.load(&e.file_name().to_string_lossy()).ok())
            .collect();
        all.sort_by(|a, b| b.id.cmp(&a.id));
        all
    }

    pub fn append_segments(&self, id: &str, segments: &[Segment]) -> Result<()> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir(id).join("transcript.jsonl"))?;
        for s in segments {
            writeln!(f, "{}", serde_json::to_string(s)?)?;
        }
        Ok(())
    }

    /// Segments sorted by time. Works mid-meeting too (the jsonl grows live).
    pub fn segments(&self, id: &str) -> Vec<Segment> {
        let raw = fs::read_to_string(self.dir(id).join("transcript.jsonl")).unwrap_or_default();
        let mut segs: Vec<Segment> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        segs.sort_by(|a, b| a.start.total_cmp(&b.start));
        segs
    }

    /// `[00:01:23] me: text` lines.
    pub fn transcript(&self, id: &str) -> String {
        render(&self.segments(id))
    }

    pub fn read(&self, id: &str, file: &str) -> Option<String> {
        fs::read_to_string(self.dir(id).join(file)).ok()
    }

    pub fn write(&self, id: &str, file: &str, content: &str) -> Result<()> {
        Ok(fs::write(self.dir(id).join(file), content)?)
    }

    pub fn wav_path(&self, id: &str, source: Source) -> PathBuf {
        self.dir(id).join(format!("{}.wav", source.label()))
    }
}

pub fn render(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| {
            let t = s.start as u64;
            format!(
                "[{:02}:{:02}:{:02}] {}: {}\n",
                t / 3600,
                t / 60 % 60,
                t % 60,
                s.source.label(),
                s.text
            )
        })
        .collect()
}
