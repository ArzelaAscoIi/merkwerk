//! Screen activity log: one JSONL file per day, `activity/YYYY-MM-DD.jsonl`.
//! Per app and window, the first entry of the day carries the full text (`text`); later entries
//! carry only the lines that are new since then (`add`). Entries without either are heartbeats
//! that keep the time accounting honest. `activity/YYYY-MM-DD.md` is the generated work log.

use crate::{
    config::Config,
    screen::{self, Snapshot},
    store::Store,
    summarize,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Write,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_secs(5);
/// Unchanged windows still get a heartbeat this often.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// No input for this long counts as away: nothing is recorded.
const IDLE_SECS: f64 = 120.0;
/// An entry accounts for the time until the next one, but at most this much.
const MAX_SPAN_SECS: i64 = 60;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub ts: String,
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    pub window: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Full window text: the first time this window is seen today, or after a big change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Lines that appeared since the previous capture of this window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<Vec<String>>,
}

impl Entry {
    /// The text this entry contributes: the full snapshot or the added lines.
    pub fn content(&self) -> String {
        match (&self.text, &self.add) {
            (Some(t), _) => t.clone(),
            (None, Some(a)) => a.join("\n"),
            (None, None) => String::new(),
        }
    }
}

/// What to store for a window whose text went from `prev` to `new`.
enum Change {
    Full,
    Added(Vec<String>),
    None,
}

fn change(prev: Option<&String>, new: &str) -> Change {
    let Some(prev) = prev else {
        return if new.is_empty() { Change::None } else { Change::Full };
    };
    if prev == new {
        return Change::None;
    }
    let old: HashSet<&str> = prev.lines().collect();
    let mut seen = HashSet::new();
    let added: Vec<String> = new
        .lines()
        .filter(|l| !old.contains(l) && seen.insert(*l))
        .map(String::from)
        .collect();
    // When most of the window is new (a different page in the same tab), a full snapshot reads better.
    if added.iter().map(String::len).sum::<usize>() * 2 > new.len() {
        Change::Full
    } else if added.is_empty() {
        Change::None // only removals: scrolled away or deleted, nothing new to keep
    } else {
        Change::Added(added)
    }
}

/// A stretch of time in one window.
#[derive(Debug)]
pub struct Session {
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub app: String,
    pub window: String,
    pub url: Option<String>,
    /// Snapshot plus the lines added during the session.
    pub text: String,
}

pub fn dir(root: &std::path::Path) -> PathBuf {
    root.join("activity")
}

fn day_file(cfg: &Config, day: NaiveDate, ext: &str) -> PathBuf {
    dir(&cfg.root).join(format!("{day}.{ext}"))
}

/// Capture loop for `merkwerk watch`; never returns. Writes yesterday's work log after midnight.
pub fn run(cfg: &Config, store: &Store) {
    let (c, s) = (cfg.clone(), store.clone());
    thread::spawn(move || backfill(&c, &s));
    // Off until switched on: no permission prompt, nothing read.
    while !crate::ui::SCREEN.load(std::sync::atomic::Ordering::Relaxed) {
        thread::sleep(TICK);
    }
    if !screen::trusted(true) {
        eprintln!("screen activity: waiting for Accessibility permission (System Settings > Privacy & Security)");
        while !screen::trusted(false) {
            thread::sleep(Duration::from_secs(10));
        }
    }
    eprintln!("screen activity: recording to {}", dir(&cfg.root).display());
    let mut prepared = HashSet::new();
    let mut last_text: HashMap<(String, String), String> = HashMap::new();
    let mut last_write: Option<(String, String, Instant)> = None;
    let mut last_snap: Option<Snapshot> = None;
    let mut today = Local::now().date_naive();
    loop {
        thread::sleep(TICK);
        let now = Local::now();
        if now.date_naive() != today {
            let (cfg, store, day) = (cfg.clone(), store.clone(), today);
            thread::spawn(move || {
                if let Err(e) = write_work_log(&cfg, &store, day) {
                    eprintln!("work log {day}: {e:#}");
                }
            });
            today = now.date_naive();
            last_text.clear();
        }
        if !crate::ui::SCREEN.load(std::sync::atomic::Ordering::Relaxed) || screen::idle_secs() > IDLE_SECS {
            continue;
        }
        // No keyboard or mouse input since the last tick and the same app in front: the walk would
        // almost always find the same text, so skip it and only keep the heartbeat going.
        if screen::idle_secs() >= TICK.as_secs_f64()
            && let Some(prev) = &last_snap
            && screen::frontmost_app().as_deref() == Some(prev.app.as_str())
        {
            if last_write.as_ref().is_some_and(|(_, _, t)| t.elapsed() >= HEARTBEAT) {
                let _ = append(cfg, today, &entry(prev, now, Change::None));
                last_write = Some((prev.app.clone(), prev.window.clone(), Instant::now()));
            }
            continue;
        }
        let Some(snap) = screen::snapshot(&mut prepared) else {
            continue;
        };
        if screen::ignored(&snap, &cfg.screen_ignore) {
            continue;
        }
        let key = (snap.app.clone(), snap.window.clone());
        let change = change(last_text.get(&key), &snap.text);
        let same_window = last_write.as_ref().is_some_and(|(a, w, _)| (a, w) == (&key.0, &key.1));
        let due = last_write.as_ref().is_none_or(|(_, _, t)| t.elapsed() >= HEARTBEAT);
        if matches!(change, Change::None) && same_window && !due {
            continue;
        }
        if let Err(e) = append(cfg, today, &entry(&snap, now, change)) {
            eprintln!("screen activity: {e:#}");
        }
        last_text.insert(key.clone(), snap.text.clone());
        last_write = Some((key.0, key.1, Instant::now()));
        last_snap = Some(snap);
    }
}

fn entry(s: &Snapshot, now: DateTime<Local>, change: Change) -> Entry {
    let (text, add) = match change {
        Change::Full => (Some(s.text.clone()), None),
        Change::Added(lines) => (None, Some(lines)),
        Change::None => (None, None),
    };
    Entry {
        ts: now.to_rfc3339(),
        app: s.app.clone(),
        bundle: s.bundle.clone(),
        window: s.window.clone(),
        url: s.url.clone(),
        text,
        add,
    }
}

fn append(cfg: &Config, day: NaiveDate, e: &Entry) -> Result<()> {
    fs::create_dir_all(dir(&cfg.root))?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(day_file(cfg, day, "jsonl"))?;
    writeln!(f, "{}", serde_json::to_string(e)?)?;
    Ok(())
}

pub fn entries(cfg: &Config, day: NaiveDate) -> Vec<Entry> {
    fs::read_to_string(day_file(cfg, day, "jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Consecutive entries in the same window, merged. Gaps (idle, locked, ignored) end a session.
pub fn sessions(entries: &[Entry]) -> Vec<Session> {
    let parse = |e: &Entry| {
        DateTime::parse_from_rfc3339(&e.ts)
            .ok()
            .map(|t| t.with_timezone(&Local))
    };
    let mut out: Vec<Session> = vec![];
    for (i, e) in entries.iter().enumerate() {
        let Some(start) = parse(e) else { continue };
        let next = entries.get(i + 1).and_then(parse);
        let span = next
            .map_or(TICK.as_secs() as i64, |n| (n - start).num_seconds())
            .clamp(0, MAX_SPAN_SECS);
        let end = start + chrono::Duration::seconds(span);
        let text = e.content();
        match out.last_mut() {
            Some(s) if s.app == e.app && s.window == e.window && (start - s.end).num_seconds() <= MAX_SPAN_SECS => {
                s.end = end;
                // Session text: the snapshot plus every line added during the session.
                if !text.is_empty() && s.text.len() < 20_000 {
                    if !s.text.is_empty() {
                        s.text.push('\n');
                    }
                    s.text.push_str(&text);
                }
                if s.url.is_none() {
                    s.url = e.url.clone();
                }
            }
            _ => out.push(Session {
                start,
                end,
                app: e.app.clone(),
                window: e.window.clone(),
                url: e.url.clone(),
                text,
            }),
        }
    }
    out
}

/// Sessions worth showing: stretches of at least `min_secs`, with neighbours in the same window
/// joined when only hidden short stretches (a quick look at another app) lay between them.
pub fn visible_sessions(sessions: Vec<Session>, min_secs: i64) -> Vec<Session> {
    let mut out: Vec<Session> = vec![];
    for s in sessions
        .into_iter()
        .filter(|s| (s.end - s.start).num_seconds() >= min_secs)
    {
        match out.last_mut() {
            Some(prev) if prev.app == s.app && prev.window == s.window && (s.start - prev.end).num_seconds() <= 120 => {
                prev.end = s.end;
                if !s.text.is_empty() && prev.text.len() < 20_000 {
                    prev.text.push('\n');
                    prev.text.push_str(&s.text);
                }
                if prev.url.is_none() {
                    prev.url = s.url;
                }
            }
            _ => out.push(s),
        }
    }
    out
}

/// Compact plain-text view of a day: time per app, then every session of 30s or more with a
/// text excerpt. Input for the work log, and what the MCP returns.
pub fn digest(cfg: &Config, day: NaiveDate, excerpt: usize) -> String {
    let sessions = sessions(&entries(cfg, day));
    if sessions.is_empty() {
        return format!("No screen activity recorded on {day}.\n");
    }
    let mut per_app: BTreeMap<&str, i64> = BTreeMap::new();
    for s in &sessions {
        *per_app.entry(&s.app).or_default() += (s.end - s.start).num_seconds();
    }
    let mut apps: Vec<_> = per_app.into_iter().collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.1));
    let mut out = format!("# Screen activity {day}\n\nTime per app: ");
    out += &apps
        .iter()
        .map(|(a, s)| format!("{a} {}", minutes(*s)))
        .collect::<Vec<_>>()
        .join(", ");
    out += "\n\n";
    for s in &visible_sessions(sessions, 30) {
        out += &format!(
            "{}-{} ({}) {} | {}{}\n",
            s.start.format("%H:%M"),
            s.end.format("%H:%M"),
            minutes((s.end - s.start).num_seconds()),
            s.app,
            s.window,
            s.url.as_deref().map(|u| format!(" | {u}")).unwrap_or_default()
        );
        let text: String = s.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if excerpt > 0 && !text.is_empty() {
            out += &format!("  > {}\n", text.chars().take(excerpt).collect::<String>());
        }
    }
    out
}

fn minutes(secs: i64) -> String {
    match secs / 60 {
        0 => "<1m".into(),
        m if m < 60 => format!("{m}m"),
        m => format!("{}h{:02}m", m / 60, m % 60),
    }
}

const WORK_LOG_PROMPT: &str = "You turn a day of recorded computer activity into a work log for {ME}.

Input: time per app, then sessions `start-end (duration) app | window title | url` with an
excerpt of the text on screen, then the day's meeting notes. Text excerpts are machine
extracted and partial. Infer what {ME} worked on; never invent work that is not in the input.

Write in English, Markdown, in this shape:

# Work log {DAY}

<2-3 sentences: what the day was about>

# Work
## <project or topic>
- what was done, concretely: tickets, PRs, documents, repos, decisions, with names and IDs from \\
the input (about <time>)

# Meetings
- **<title>** (<time>): one line outcome

# Communication
- notable Slack, email or chat threads: who, what about

# Open threads
- things started but not finished, follow-ups visible in the input

Group by project, not by app. Skip entertainment and idle noise. Omit empty sections. No preamble.";

/// Writes the work log of every past day that has activity but no log, e.g. when merkwerk wasn't
/// running at midnight.
fn backfill(cfg: &Config, store: &Store) {
    let today = Local::now().date_naive();
    let Ok(dir) = fs::read_dir(dir(&cfg.root)) else {
        return;
    };
    let mut days: Vec<NaiveDate> = dir
        .flatten()
        .filter_map(|e| e.path().file_name()?.to_str()?.strip_suffix(".jsonl")?.parse().ok())
        .filter(|d| *d < today && !day_file(cfg, *d, "md").exists())
        .collect();
    days.sort();
    for day in days {
        if let Err(e) = write_work_log(cfg, store, day) {
            eprintln!("work log {day}: {e:#}");
        }
    }
}

/// Writes and returns `activity/<day>.md` from the day's screen activity and meeting notes.
pub fn write_work_log(cfg: &Config, store: &Store, day: NaiveDate) -> Result<String> {
    let mut input = digest(cfg, day, 400);
    // Limitation: one request; a very busy day can exceed small context windows. Chunk by hour if so.
    const MAX_INPUT: usize = 120_000;
    if input.len() > MAX_INPUT {
        input.truncate(input.char_indices().nth(MAX_INPUT).map_or(input.len(), |(i, _)| i));
    }
    let prefix = day.format("%Y-%m-%d").to_string();
    for m in store.list().iter().filter(|m| m.id.starts_with(&prefix)) {
        input += &format!("\n## Meeting {} ({})\n", m.title, m.id.get(11..15).unwrap_or(""));
        input += &store.read(&m.id, "summary.md").unwrap_or_default();
    }
    let log = summarize::chat(
        cfg,
        &WORK_LOG_PROMPT
            .replace("{ME}", &cfg.me)
            .replace("{DAY}", &day.format("%A, %Y-%m-%d").to_string()),
        &input,
    )
    .context("writing work log")?;
    fs::create_dir_all(dir(&cfg.root))?;
    fs::write(day_file(cfg, day, "md"), &log)?;
    Ok(log)
}

pub fn read_work_log(cfg: &Config, day: NaiveDate) -> Option<String> {
    fs::read_to_string(day_file(cfg, day, "md")).ok()
}

/// Case-insensitive search over every day's captured text, window titles and URLs, newest first.
pub fn search(cfg: &Config, query: &str, limit: usize) -> Vec<(String, String, String)> {
    let q = query.to_lowercase();
    let mut days: Vec<PathBuf> = fs::read_dir(dir(&cfg.root))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    days.sort();
    let mut hits = vec![];
    for path in days.iter().rev() {
        let raw = fs::read_to_string(path).unwrap_or_default();
        for e in raw.lines().rev().filter_map(|l| serde_json::from_str::<Entry>(l).ok()) {
            let head = format!("{} | {} | {}", e.app, e.window, e.url.as_deref().unwrap_or(""));
            let text = e.content();
            let line = text.lines().find(|l| l.to_lowercase().contains(&q));
            if line.is_some() || head.to_lowercase().contains(&q) {
                hits.push((e.ts.clone(), head, line.unwrap_or("").chars().take(300).collect()));
                if hits.len() >= limit {
                    return hits;
                }
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(ts: &str, app: &str, window: &str, text: Option<&str>) -> Entry {
        Entry {
            ts: format!("2026-10-17T{ts}+02:00"),
            app: app.into(),
            bundle: None,
            window: window.into(),
            url: None,
            text: text.map(String::from),
            add: None,
        }
    }

    #[test]
    fn stores_full_text_first_then_only_new_lines() {
        assert!(matches!(change(None, "a\nb"), Change::Full));
        let prev = "inbox\nmessage one\nmessage two\nfooter".to_string();
        match change(Some(&prev), "inbox\nmessage two\nmessage three\nfooter") {
            Change::Added(lines) => assert_eq!(lines, ["message three"]),
            _ => panic!("expected only the new line"),
        }
        assert!(
            matches!(change(Some(&prev), "inbox\nfooter"), Change::None),
            "removals store nothing"
        );
        assert!(matches!(
            change(Some(&prev), "a completely different page with lots of new text"),
            Change::Full
        ));
    }

    #[test]
    fn hidden_blips_do_not_split_a_window() {
        let s = visible_sessions(
            sessions(&[
                e("09:00:00", "Vivaldi", "Docs", Some("a")),
                e("09:00:30", "Vivaldi", "Docs", None),
                e("09:01:00", "Orbit", "Orbit", None), // 10 s look at another app
                e("09:01:10", "Vivaldi", "Docs", Some("b")),
                e("09:01:40", "Vivaldi", "Docs", None),
                e("09:02:10", "Vivaldi", "Docs", None),
            ]),
            30,
        );
        assert_eq!(s.len(), 1, "one Vivaldi row, the Orbit blip hidden");
        assert_eq!((s[0].end - s[0].start).num_seconds(), 135, "last entry counts one tick");
    }

    #[test]
    fn sessions_merge_and_split_on_gaps() {
        let s = sessions(&[
            e("09:00:00", "Vivaldi", "Linear", Some("ECO-1")),
            e("09:00:30", "Vivaldi", "Linear", None),
            e("09:01:00", "Vivaldi", "Linear", Some("ECO-1 longer text")),
            e("09:01:20", "Ghostty", "zsh", None),
            e("09:30:00", "Ghostty", "zsh", None), // gap of ~29 min: away
        ]);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].end - s[0].start).num_seconds(), 80);
        assert_eq!(s[0].text, "ECO-1\nECO-1 longer text");
        assert_eq!((s[1].end - s[1].start).num_seconds(), 60, "capped span before the gap");
    }
}
