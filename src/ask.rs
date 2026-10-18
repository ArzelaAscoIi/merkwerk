//! Chat about one meeting or one day: questions answered from the transcript (live while the
//! meeting runs) or from the day's activity, by the configured local LLM.

use crate::{activity, config::Config, store::Store, summarize};
use anyhow::{Context, Result};
use serde_json::{Value, json};

const MEETING_PROMPT: &str = "You answer {ME}'s questions about one meeting, using only the transcript and notes below.
Transcript lines are `[hh:mm:ss] speaker: text`; `me` is {ME}, `them` is everyone else. The
transcript is machine generated and may contain recognition errors.

Be brief and concrete. Cite the moment for key points as [hh:mm:ss]. If the answer is not in the
transcript, say so.";

const DAY_PROMPT: &str = "You answer {ME}'s questions about one workday, using only the work log and the
screen activity below (sessions `start-end (duration) app | window | url` with text excerpts).
Be brief and concrete; name tickets, documents and people from the input. If something is not in
the input, say so.";

/// `target` is a meeting id, or `<YYYY-MM-DD>_log` for a day. `history` holds earlier
/// (question, answer) pairs of this chat. Returns the answer as Markdown.
pub fn answer(
    cfg: &Config,
    store: &Store,
    target: &str,
    history: &[(String, String)],
    question: &str,
) -> Result<String> {
    let (prompt, context) = match target.strip_suffix("_log") {
        Some(day) => {
            let day = day.parse().context("bad day")?;
            let log = activity::read_work_log(cfg, day).unwrap_or_default();
            (
                DAY_PROMPT,
                format!("# Work log\n{log}\n\n{}", activity::digest(cfg, day, 300)),
            )
        }
        None => {
            let meta = store.load(target)?;
            let mut context = format!("# Meeting: {}\n", meta.title);
            if meta.ended_at.is_none() {
                let elapsed = chrono::DateTime::parse_from_rfc3339(&meta.started_at)
                    .map(|s| (chrono::Local::now().fixed_offset() - s).num_seconds().max(0))
                    .unwrap_or(0);
                context += &format!(
                    "The meeting is still running; it is now [{:02}:{:02}:{:02}]. The transcript lags up to 30 s.\n",
                    elapsed / 3600,
                    elapsed / 60 % 60,
                    elapsed % 60
                );
            }
            if let Some(notes) = store.read(target, "summary.md") {
                context += &format!("\n## Notes\n{notes}\n");
            }
            context += &format!("\n## Transcript\n{}", store.transcript(target));
            (MEETING_PROMPT, context)
        }
    };
    let mut messages =
        vec![json!({"role": "system", "content": format!("{}\n\n{context}", prompt.replace("{ME}", &cfg.me))})];
    for (q, a) in history {
        messages.push(json!({"role": "user", "content": q}));
        messages.push(json!({"role": "assistant", "content": a}));
    }
    // Small models drift into other languages from a system-prompt rule; pinning it to the
    // question itself keeps English questions in English and German ones in German.
    messages.push(json!({"role": "user", "content": format!("{question}\n\n(Reply in the language this question is written in.)")}));
    summarize::chat_messages(cfg, Value::Array(messages))
}
