//! Plain files, one directory per meeting:
//!   meta.json  me.wav  them.wav  transcript.jsonl  transcript.md  summary.md

use crate::audio::Source;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, io::Write, path::PathBuf};

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
    /// Names invited to the calendar event the recording overlaps, without the recorder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
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
            attendees: vec![],
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

    /// Segments sorted by time, echo dropped. Works mid-meeting too (the jsonl grows live).
    pub fn segments(&self, id: &str) -> Vec<Segment> {
        let raw = fs::read_to_string(self.dir(id).join("transcript.jsonl")).unwrap_or_default();
        let mut segs: Vec<Segment> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        segs.sort_by(|a, b| a.start.total_cmp(&b.start));
        drop_echo(segs)
    }

    /// `[00:01:23] me: text` lines; in a 1:1 `them` is the other person's first name.
    pub fn transcript(&self, id: &str) -> String {
        let text = render(&self.segments(id));
        match self.them_name(id) {
            Some(name) => text.replace("] them: ", &format!("] {name}: ")),
            None => text,
        }
    }

    /// The first name of the only other invitee, when the calendar event had exactly one: then all
    /// system audio is that person.
    pub fn them_name(&self, id: &str) -> Option<String> {
        match self.load(id).ok()?.attendees.as_slice() {
            [one] => first_name(one),
            _ => None,
        }
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

/// The mic also hears the speakers, so remote speech lands twice: on `them` from system audio and,
/// slightly garbled, on `me`. Drops `me` segments whose words mostly occur in `them` speech around
/// the same time. Raw segments stay in transcript.jsonl.
// ponytail: word-overlap heuristic; switch the mic to Apple's voice-processing unit (real echo
// cancellation) if it drops genuine `me` speech that echoes what was just said.
fn drop_echo(segs: Vec<Segment>) -> Vec<Segment> {
    const SLACK: f64 = 3.0; // seconds; the two streams drift a little
    fn words(t: &str) -> Vec<String> {
        t.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    }
    let them: Vec<(f64, f64, HashSet<String>)> = segs
        .iter()
        .filter(|s| s.source == Source::Them)
        .map(|s| (s.start, s.end, words(&s.text).into_iter().collect()))
        .collect();
    segs.into_iter()
        .filter(|s| {
            if s.source != Source::Me {
                return true;
            }
            let near: HashSet<&String> = them
                .iter()
                .filter(|(start, end, _)| *start <= s.end + SLACK && *end >= s.start - SLACK)
                .flat_map(|(_, _, w)| w)
                .collect();
            let w = words(&s.text);
            let echoed = w.iter().filter(|x| near.contains(x)).count();
            w.is_empty() || echoed * 10 < w.len() * 6
        })
        .collect()
}

/// "Ada Lovelace" -> "Ada"; an invitee without a display name is an address: "ada.l@co" -> "ada.l".
fn first_name(invitee: &str) -> Option<String> {
    invitee
        .split(|c: char| c.is_whitespace() || c == '@')
        .find(|w| !w.is_empty())
        .map(str::to_string)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_name_of_invitee() {
        assert_eq!(first_name("Ada Lovelace").as_deref(), Some("Ada"));
        assert_eq!(first_name("ada.l@example.com").as_deref(), Some("ada.l"));
        assert_eq!(first_name("  "), None);
    }

    fn seg(source: Source, start: f64, text: &str) -> Segment {
        Segment {
            source,
            start,
            end: start + 4.0,
            text: text.into(),
        }
    }

    #[test]
    fn drops_mic_echo_of_remote_speech_only() {
        let segs = vec![
            seg(
                Source::Them,
                0.0,
                "The build finished green, and then the deploy went out at noon.",
            ),
            seg(Source::Me, 1.0, "The build finish green."), // echo, slightly garbled
            seg(Source::Me, 2.5, "And then the deploy went out at noon."), // echo
            seg(Source::Me, 30.0, "Which region did it roll out to first?"), // own speech, them silent
            seg(Source::Me, 3.0, "Wait, was that the staging cluster?"), // own speech over them
        ];
        let kept: Vec<_> = drop_echo(segs).into_iter().map(|s| s.text).collect();
        assert_eq!(kept.len(), 3, "{kept:?}");
        assert!(kept.iter().any(|t| t.contains("region")));
        assert!(kept.iter().any(|t| t.contains("staging")));
    }
}
