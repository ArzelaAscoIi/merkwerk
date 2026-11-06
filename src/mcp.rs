//! MCP server over stdio (JSON-RPC 2.0, one message per line). Read-only access to meetings.

use crate::{activity, config::Config, store::Store};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const PROTOCOL: &str = "2025-06-18";
const DEFAULT_LIMIT: usize = 1000;

pub fn serve(cfg: &Config, store: &Store) -> Result<()> {
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(m) => handle(cfg, store, &m),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(msg) = msg {
            writeln!(out, "{msg}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Response for one incoming message; None for notifications.
pub fn handle(cfg: &Config, store: &Store, msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "merkwerk", "version": env!("CARGO_PKG_VERSION")},
            "instructions": instructions(store),
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(
            match call(
                cfg,
                store,
                params["name"].as_str().unwrap_or_default(),
                &params["arguments"],
            ) {
                Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
            },
        ),
        m => Err(json!({"code": -32601, "message": format!("unknown method {m}")})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

/// Sent once per session: how to keep corrections, and the ones already saved for all meetings.
fn instructions(store: &Store) -> String {
    let mut text = String::from(
        "merkwerk holds the user's meetings, screen activity and work logs. Transcripts come from local \
speech recognition, so names and terms can be misheard, and `them` is everyone on the other end. When the \
user corrects something (a misheard name, who said what, who someone is, missing background), save it \
with add_context: for all meetings, or for one with meeting_id. Summaries, answers and later sessions then \
use it.",
    );
    let notes = store.context(None);
    if !notes.is_empty() {
        text += "\n\nSaved corrections (always true):";
        for n in notes {
            text += &format!("\n- {n}");
        }
    }
    text
}

fn tools() -> Value {
    json!([
        {
            "name": "list_meetings",
            "description": "List recorded meetings, newest first. Returns id, title, tags, start/end time and the app that triggered the recording.",
            "inputSchema": {"type": "object", "properties": {
                "since": {"type": "string", "description": "Only meetings on or after this date (YYYY-MM-DD)."},
                "tag": {"type": "string", "description": "Only meetings with this tag (case-insensitive)."},
                "limit": {"type": "integer", "description": "Max meetings to return (default 50)."},
            }},
        },
        {
            "name": "get_meetings",
            "description": "Metadata and AI summary (notes, decisions, action items) for meetings by id.",
            "inputSchema": {"type": "object", "required": ["meeting_ids"], "properties": {
                "meeting_ids": {"type": "array", "items": {"type": "string"}},
            }},
        },
        {
            "name": "get_meeting_transcript",
            "description": "Full verbatim transcript, one `[hh:mm:ss] speaker: text` line per utterance. `me` is the recording user (microphone), `them` is everyone else (system audio). Works while a meeting is still running. Paged by line: pass next_offset to continue.",
            "inputSchema": {"type": "object", "required": ["meeting_id"], "properties": {
                "meeting_id": {"type": "string"},
                "offset": {"type": "integer", "description": "First line to return (default 0)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 1000)."},
            }},
        },
        {
            "name": "get_activity",
            "description": "What the user did on their computer on a day: time per app, then every window session (start-end, app, window title, URL) with an excerpt of the on-screen text. Text comes from the macOS Accessibility API.",
            "inputSchema": {"type": "object", "properties": {
                "date": {"type": "string", "description": "YYYY-MM-DD (default today)."},
                "excerpt_chars": {"type": "integer", "description": "Text excerpt per session (default 200, 0 for none)."},
            }},
        },
        {
            "name": "get_work_log",
            "description": "The AI-written work log for a day: projects worked on with concrete artifacts, meetings, communication, open threads. Written from screen activity and meeting notes; generated on first request (can take a minute).",
            "inputSchema": {"type": "object", "properties": {
                "date": {"type": "string", "description": "YYYY-MM-DD (default today)."},
                "refresh": {"type": "boolean", "description": "Regenerate even if a log exists."},
            }},
        },
        {
            "name": "search_activity",
            "description": "Case-insensitive search over all captured on-screen text, window titles and URLs, newest first. Returns timestamp, app | window | url, and the matching line.",
            "inputSchema": {"type": "object", "required": ["query"], "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "Max hits (default 30)."},
            }},
        },
        {
            "name": "add_context",
            "description": "Save a correction or background note so merkwerk gets it right from now on: a misheard name ('Kjartur is Hjoetur'), who a speaker is ('them in this meeting is Anna from Acme'), who someone is ('Robin leads the Agents team') or context a summary missed. Without meeting_id it applies to all meetings and work logs; with it, to that meeting only. Used by every later summary, answer and MCP session. Set rewrite_notes to rewrite that meeting's notes with it now (takes a minute).",
            "inputSchema": {"type": "object", "required": ["note"], "properties": {
                "note": {"type": "string", "description": "One plain sentence, written as a fact."},
                "meeting_id": {"type": "string", "description": "Only for this meeting (default: all)."},
                "rewrite_notes": {"type": "boolean", "description": "With meeting_id: rewrite its notes now."},
            }},
        },
        {
            "name": "remove_context",
            "description": "Delete a saved correction that turned out wrong, by its exact text (as listed in the server instructions or get_meetings).",
            "inputSchema": {"type": "object", "required": ["note"], "properties": {
                "note": {"type": "string"},
                "meeting_id": {"type": "string", "description": "The meeting it was saved for (default: all meetings)."},
            }},
        },
        {
            "name": "search_meetings",
            "description": "Case-insensitive text search across all transcripts and summaries. Returns matching lines per meeting.",
            "inputSchema": {"type": "object", "required": ["query"], "properties": {
                "query": {"type": "string"},
                "tag": {"type": "string", "description": "Only search meetings with this tag (case-insensitive)."},
            }},
        },
    ])
}

fn call(cfg: &Config, store: &Store, name: &str, args: &Value) -> Result<String> {
    let text = match name {
        "list_meetings" => {
            let since = args["since"].as_str().unwrap_or("");
            let tag = args["tag"].as_str();
            let limit = args["limit"].as_u64().unwrap_or(50) as usize;
            let list: Vec<_> = store
                .list()
                .into_iter()
                .filter(|m| m.id.as_str() >= since && tag.is_none_or(|t| m.has_tag(t)))
                .take(limit)
                .collect();
            serde_json::to_string_pretty(&list)?
        }
        "get_meetings" => {
            let ids = args["meeting_ids"].as_array().context("meeting_ids required")?;
            let mut out = vec![];
            for id in ids.iter().filter_map(Value::as_str) {
                out.push(match store.load(id) {
                    Ok(meta) => json!({
                        "meta": meta,
                        "summary": store.read(id, "summary.md"),
                        "corrections": store.context(Some(id)),
                    }),
                    Err(e) => json!({"id": id, "error": e.to_string()}),
                });
            }
            serde_json::to_string_pretty(&out)?
        }
        "get_meeting_transcript" => {
            let id = args["meeting_id"].as_str().context("meeting_id required")?;
            store.load(id)?;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let limit = args["limit"].as_u64().map_or(DEFAULT_LIMIT, |l| l as usize);
            // The raw transcript keeps its mistakes; the corrections go on top of the first page.
            let fixes = store.context_prompt(Some(id));
            let head = if offset == 0 && !fixes.is_empty() {
                format!("{}\n\n", fixes.trim_start())
            } else {
                String::new()
            };
            head + &page(&store.transcript(id), offset, limit)
        }
        "search_meetings" => {
            let q = args["query"].as_str().context("query required")?.to_lowercase();
            let mut out = String::new();
            let tag = args["tag"].as_str();
            for meta in store.list().into_iter().filter(|m| tag.is_none_or(|t| m.has_tag(t))) {
                let text = store.read(&meta.id, "summary.md").unwrap_or_default() + &store.transcript(&meta.id);
                let hits: Vec<&str> = text
                    .lines()
                    .filter(|l| l.to_lowercase().contains(&q))
                    .take(10)
                    .collect();
                if !hits.is_empty() {
                    out += &format!("## {} | {}\n{}\n\n", meta.id, meta.title, hits.join("\n"));
                }
            }
            if out.is_empty() { "no matches".into() } else { out }
        }
        "add_context" => {
            let note = args["note"].as_str().context("note required")?;
            let id = args["meeting_id"].as_str();
            store.add_context(id, note)?;
            match id {
                Some(id) if args["rewrite_notes"].as_bool().unwrap_or(false) => {
                    // finalize deletes a meeting with an empty transcript; a running one finishes on its own.
                    anyhow::ensure!(
                        !store.live(&store.load(id)?),
                        "Saved; the notes are written when the meeting ends."
                    );
                    anyhow::ensure!(
                        !store.transcript(id).trim().is_empty(),
                        "Saved; this meeting has no transcript to rewrite."
                    );
                    crate::session::finalize(cfg, store, id)?;
                    format!(
                        "Saved for {id}; notes rewritten:\n\n{}",
                        store.read(id, "summary.md").unwrap_or_default()
                    )
                }
                Some(id) => format!("Saved for {id}."),
                None => "Saved for all meetings.".into(),
            }
        }
        "remove_context" => {
            let note = args["note"].as_str().context("note required")?;
            if store.remove_context(args["meeting_id"].as_str(), note)? {
                "Removed.".into()
            } else {
                bail!("no saved correction with exactly that text")
            }
        }
        "get_activity" => {
            let excerpt = args["excerpt_chars"].as_u64().unwrap_or(200) as usize;
            activity::digest(cfg, date(args)?, excerpt)
        }
        "get_work_log" => {
            let day = date(args)?;
            match activity::read_work_log(cfg, day) {
                Some(log) if !args["refresh"].as_bool().unwrap_or(false) => log,
                _ => activity::write_work_log(cfg, store, day)?,
            }
        }
        "search_activity" => {
            let q = args["query"].as_str().context("query required")?;
            let limit = args["limit"].as_u64().unwrap_or(30) as usize;
            let hits = activity::search(cfg, q, limit);
            if hits.is_empty() {
                "no matches".into()
            } else {
                hits.iter()
                    .map(|(ts, head, line)| format!("{ts} | {head}\n  {line}\n"))
                    .collect()
            }
        }
        _ => bail!("unknown tool {name}"),
    };
    Ok(text)
}

fn date(args: &Value) -> Result<chrono::NaiveDate> {
    match args["date"].as_str() {
        Some(d) => Ok(chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").context("date must be YYYY-MM-DD")?),
        None => Ok(chrono::Local::now().date_naive()),
    }
}

fn page(text: &str, offset: usize, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let end = (offset + limit).min(lines.len());
    let mut out = lines.get(offset..end).unwrap_or_default().join("\n");
    if end < lines.len() {
        out += &format!("\n\n[truncated: {} lines total, next_offset: {end}]", lines.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_lines() {
        let text = "a\nb\nc";
        assert_eq!(page(text, 0, 10), "a\nb\nc");
        assert_eq!(page(text, 0, 2), "a\nb\n\n[truncated: 3 lines total, next_offset: 2]");
        assert_eq!(page(text, 2, 2), "c");
        assert_eq!(page(text, 9, 2), "");
    }

    #[test]
    fn notifications_get_no_reply() {
        let store = Store {
            root: "/nonexistent".into(),
        };
        let cfg = Config::from_env();
        assert!(
            handle(
                &cfg,
                &store,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
            )
            .is_none()
        );
        let r = handle(
            &cfg,
            &store,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 9);
    }
}

#[cfg(test)]
mod tag_tests {
    use super::*;

    #[test]
    fn tags_normalize_and_filter_meetings() {
        let root = std::env::temp_dir().join(format!("merkwerk-tags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store { root: root.clone() };
        let a = store.create("A", None).unwrap();
        let meta = store
            .set_tags(&a.id, &[" Sales ".into(), "sales".into(), "".into(), "Q4".into()])
            .unwrap();
        assert_eq!(meta.tags, ["Sales", "Q4"]);
        assert!(store.set_tags("../x", &[]).is_err());
        assert_eq!(
            store.set_title(&a.id, "  Pipeline review ").unwrap().title,
            "Pipeline review"
        );
        assert!(store.set_title(&a.id, " ").is_err());
        assert!(store.set_title("../x", "y").is_err());
        // Trash guards run before anything moves: bad ids and live recordings are refused.
        assert!(store.trash("../x").is_err());
        std::fs::write(store.wav_path(&a.id, crate::audio::Source::Me), b"").unwrap();
        assert!(store.trash(&a.id).unwrap_err().to_string().contains("still recording"));
        let cfg = Config::from_env();
        let tagged = call(&cfg, &store, "list_meetings", &json!({"tag": "SALES"})).unwrap();
        assert!(tagged.contains(&a.id));
        let none = call(&cfg, &store, "list_meetings", &json!({"tag": "hr"})).unwrap();
        assert_eq!(none, "[]");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn corrections_are_saved_listed_and_removed() {
        let root = std::env::temp_dir().join(format!("merkwerk-context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store { root: root.clone() };
        let cfg = Config::from_env();
        let m = store.create("Sync", None).unwrap();
        let add = |args: Value| call(&cfg, &store, "add_context", &args).unwrap();
        add(json!({"note": "Kjartur is  Hjoetur"}));
        add(json!({"note": "Kjartur is Hjoetur"}));
        add(json!({"note": "them is Anna", "meeting_id": m.id}));
        assert_eq!(
            store.context(None),
            ["Kjartur is Hjoetur"],
            "whitespace normalized, repeat kept once"
        );
        assert!(instructions(&store).contains("- Kjartur is Hjoetur"));
        let p = store.context_prompt(Some(&m.id));
        assert!(p.contains("Kjartur is Hjoetur") && p.contains("them is Anna"));
        assert!(
            !store.context_prompt(None).contains("Anna"),
            "meeting notes stay with their meeting"
        );
        let got = call(&cfg, &store, "get_meetings", &json!({"meeting_ids": [m.id]})).unwrap();
        assert!(got.contains("them is Anna"));
        assert!(
            call(
                &cfg,
                &store,
                "add_context",
                &json!({"note": "x", "meeting_id": "../etc"})
            )
            .is_err()
        );
        call(&cfg, &store, "remove_context", &json!({"note": "Kjartur is Hjoetur"})).unwrap();
        assert!(store.context(None).is_empty());
        assert!(call(&cfg, &store, "remove_context", &json!({"note": "nope"})).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
