//! The notes page: one self-contained HTML document (no network, no external assets)
//! with every meeting's notes and transcript embedded as JSON.

use crate::{
    activity,
    config::Config,
    store::{Meta, Store},
};
use chrono::{DateTime, FixedOffset, Local};
use pulldown_cmark::{Event, Parser, html};
use serde_json::{Value, json};

const TEMPLATE: &str = include_str!("notes.html");

/// Cheap change detector for the page data: sizes and modification times of the files it is
/// built from (one `stat` each, no reads).
pub fn fingerprint(cfg: &Config, store: &Store) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut add = |path: &std::path::Path| {
        if let Ok(md) = std::fs::metadata(path) {
            path.hash(&mut h);
            md.len().hash(&mut h);
            md.modified().ok().hash(&mut h);
        }
    };
    // Only whether audio is still being written matters (the "Recording" badge), not each flush.
    let mut live = vec![];
    for dir in std::fs::read_dir(&store.root).into_iter().flatten().flatten() {
        for file in ["meta.json", "summary.md", "transcript.jsonl"] {
            add(&dir.path().join(file));
        }
        live.push(recent(&dir.path().join("me.wav")));
    }
    for f in std::fs::read_dir(activity::dir(&cfg.root))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = f.path();
        if path.extension().is_some_and(|x| x == "md") {
            add(&path);
            add(&path.with_extension("jsonl"));
        }
    }
    // Today's activity grows every few seconds; refresh its entry once a minute.
    let today = activity::dir(&cfg.root).join(format!("{}.jsonl", Local::now().date_naive()));
    let minute = std::fs::metadata(&today)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() / 60);
    minute.hash(&mut h);
    live.hash(&mut h);
    crate::ui::FINISHING.lock().unwrap().hash(&mut h);
    // "Today" and "Yesterday" labels change at midnight.
    Local::now().date_naive().hash(&mut h);
    h.finish()
}

/// The full page around `data` (from [`data`]).
pub fn page(data: &str) -> String {
    TEMPLATE.replace("/*DATA*/", data)
}

/// Each meeting's JSON, keyed by the sizes and mtimes of its files, its live flag and the day:
/// during a recording only the meeting being recorded is rebuilt.
static MEETINGS: std::sync::Mutex<Option<std::collections::HashMap<String, (u64, Value)>>> =
    std::sync::Mutex::new(None);

/// All meetings as JSON, safe to embed in a `<script>` tag.
// ponytail: the whole array is still serialized and sent to the page on each change; send only
// the changed meeting if the archive grows to thousands.
pub fn data(cfg: &Config, store: &Store) -> String {
    use std::hash::{Hash, Hasher};
    let today = Local::now().date_naive();
    let mut cache = MEETINGS.lock().unwrap();
    let cache = cache.get_or_insert_with(Default::default);
    let mut meetings: Vec<Value> = store
        .list()
        .iter()
        .map(|m| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for file in ["meta.json", "summary.md", "transcript.jsonl"] {
                let md = std::fs::metadata(store.dir(&m.id).join(file)).ok();
                md.as_ref().map(|md| (md.len(), md.modified().ok())).hash(&mut h);
            }
            (recently_written(store, m), writing(&m.id), today).hash(&mut h);
            let key = h.finish();
            match cache.get(&m.id) {
                Some((k, v)) if *k == key => v.clone(),
                _ => {
                    let v = meeting(store, m, today);
                    cache.insert(m.id.clone(), (key, v.clone()));
                    v
                }
            }
        })
        .collect();
    meetings.extend(work_logs(cfg, today));
    // Newest first; a day's work log (`<date>_log`) sorts above that day's meetings.
    meetings.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    // `<` never occurs outside JSON strings, so escaping it keeps `</script>` in content harmless.
    Value::Array(meetings).to_string().replace('<', "\\u003c")
}

fn meeting(store: &Store, m: &Meta, today: chrono::NaiveDate) -> Value {
    let parse = |s: &str| DateTime::parse_from_rfc3339(s).ok();
    let start = parse(&m.started_at);
    let end = m.ended_at.as_deref().and_then(parse);
    let summary = store.read(&m.id, "summary.md").unwrap_or_default();
    let segments = store.segments(&m.id);
    let them = store.them_name(&m.id);
    let lines: Vec<Value> = segments
        .iter()
        .map(|s| {
            let t = s.start as u64;
            let who = match (&them, s.source.label()) {
                (Some(name), "them") => name.as_str(),
                (_, label) => label,
            };
            json!({"at": format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60), "who": who, "text": s.text})
        })
        .collect();
    let search = format!(
        "{} {} {}",
        m.title,
        summary,
        segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ")
    )
    .to_lowercase();
    json!({
        "id": m.id,
        "title": m.title,
        "day": start.map(|s| day_label(s, today)).unwrap_or_default(),
        "date": start.map(|s| s.format("%A, %b %-d").to_string()),
        "time": start.map(|s| s.format("%H:%M").to_string()),
        "span": match (start, end) {
            (Some(a), Some(b)) => Some(format!("{} to {}", a.format("%H:%M"), b.format("%H:%M"))),
            _ => None,
        },
        "length": match (start, end) {
            (Some(a), Some(b)) => format!("{} min", (b - a).num_minutes().max(1)),
            _ => String::new(),
        },
        "app": m.app.as_deref().map(app_name),
        "live": m.ended_at.is_none() && recently_written(store, m),
        "writing": writing(&m.id),
        "notes": (!summary.trim().is_empty()).then(|| render(&summary)),
        "lines": lines,
        "search": search,
    })
}

/// One entry per day with a work log, plus today while it is recorded; its raw view lists the
/// window sessions.
fn work_logs(cfg: &Config, today: chrono::NaiveDate) -> Vec<Value> {
    let Ok(dir) = std::fs::read_dir(activity::dir(&cfg.root)) else {
        return vec![];
    };
    let mut out = vec![];
    for path in dir.flatten().map(|e| e.path()) {
        let Some(day) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<chrono::NaiveDate>().ok())
        else {
            continue;
        };
        let md = path.extension().is_some_and(|x| x == "md");
        let live =
            day == today && path.extension().is_some_and(|x| x == "jsonl") && !path.with_extension("md").exists();
        if !md && !live {
            continue;
        }
        let log = if md {
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        let sessions = activity::visible_sessions(activity::sessions(&activity::entries(cfg, day)), 30);
        let lines: Vec<Value> = sessions
            .iter()
            .map(|s| {
                json!({
                    "at": s.start.format("%H:%M").to_string(),
                    "who": s.app,
                    "text": format!("{} ({} min)", s.window, (s.end - s.start).num_minutes().max(1)),
                })
            })
            .collect();
        let start = sessions.first().map(|s| s.start.format("%H:%M").to_string());
        let end = sessions.last().map(|s| s.end.format("%H:%M").to_string());
        out.push(json!({
            "id": format!("{day}_log"),
            "kind": "log",
            "title": "Work log",
            "day": day_label_naive(day, today),
            "date": day.format("%A, %b %-d").to_string(),
            "time": start.clone().unwrap_or_default(),
            "span": start.zip(end).map(|(a, b)| format!("{a} to {b}")),
            "length": format!("{} sessions", lines.len()),
            "app": null,
            "live": false,
            "today": live,
            "notes": render(&log),
            "lines": lines,
            "search": log.to_lowercase(),
        }));
    }
    out
}

fn day_label_naive(d: chrono::NaiveDate, today: chrono::NaiveDate) -> String {
    match (today - d).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => d.format("%A, %b %-d").to_string(),
    }
}

fn day_label(start: DateTime<FixedOffset>, today: chrono::NaiveDate) -> String {
    day_label_naive(start.with_timezone(&Local).date_naive(), today)
}

/// No end time and audio written in the last seconds; a crashed recording has no end time either.
/// Whether this app is still transcribing / summarizing the meeting.
fn writing(id: &str) -> bool {
    crate::ui::FINISHING.lock().unwrap().iter().any(|f| f == id)
}

fn recently_written(store: &Store, m: &Meta) -> bool {
    recent(&store.wav_path(&m.id, crate::audio::Source::Me))
}

fn recent(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .and_then(|md| md.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e.as_secs() < 15))
}

/// "Vivaldi Helper" -> "Vivaldi", "zoom.us" stays.
fn app_name(app: &str) -> String {
    app.split(" Helper").next().unwrap_or(app).to_string()
}

/// Markdown to HTML without the leading `# title` (shown as the page title) and with any raw
/// HTML in the notes escaped: notes come from an LLM reading meeting audio, so treat as untrusted.
fn render(summary: &str) -> String {
    let body = match summary.trim_start().strip_prefix("# ") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, b)| b),
        None => summary,
    };
    markdown(body)
}

/// Markdown to HTML with raw HTML escaped (LLM output is untrusted).
pub fn markdown(body: &str) -> String {
    let events = Parser::new(body).map(|e| match e {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        e => e,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_drops_title_and_escapes_html() {
        let out = render("# Title\n\n# Topic\n- a <script>x</script>\n");
        assert!(!out.contains("Title"));
        assert!(out.contains("<h1>Topic</h1>"));
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn data_rebuilds_a_meeting_when_its_notes_change() {
        let root = std::env::temp_dir().join(format!("merkwerk-notes-test-{}", std::process::id()));
        let store = Store { root: root.clone() };
        let mut cfg = Config::from_env();
        cfg.root = root.clone();
        let m = store.create("Sync", None).unwrap();
        store
            .write(&m.id, "summary.md", "# Sync\n\n# Topic\n- first\n")
            .unwrap();
        assert!(data(&cfg, &store).contains("first"));
        store
            .write(&m.id, "summary.md", "# Sync\n\n# Topic\n- second, longer\n")
            .unwrap();
        let after = data(&cfg, &store);
        assert!(after.contains("second") && !after.contains("first"), "{after}");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod preview {
    #[test]
    #[ignore = "writes a preview page for screenshots"]
    fn write_preview() {
        let cfg = crate::config::Config::from_env();
        let store = crate::store::Store { root: cfg.root.clone() };
        std::fs::write(std::env::var("OUT").unwrap(), super::page(&super::data(&cfg, &store))).unwrap();
    }
}
