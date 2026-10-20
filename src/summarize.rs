//! Meeting notes from a transcript via any OpenAI-compatible chat endpoint.

use crate::{config::Config, store::Meta};
use anyhow::{Context, Result};
use serde_json::{Value, json};

const PROMPT: &str = "You write meeting notes from a raw transcript.

Transcript lines are `[hh:mm:ss] speaker: text`. `me` is {ME}, who recorded the meeting; `them` is everyone \
else (one or several people; use names when the conversation or the invite list reveals them). The transcript is machine \
generated and can be German, English or mixed, with recognition errors: infer intent, never invent facts.

Always write the notes in English, in Markdown, in this shape:

# <short descriptive title for the whole meeting>

# Next Steps
- **<task, starting with a verb>** (<owner first name>, <due date>)

# Decisions
- <one line per decision actually agreed>

# <topic name>
- <fact>

The reader skims: actions first, then decisions, then the few facts worth keeping.
Rules:
- Only `# ` headings, no `##`.
- Next Steps: every concrete commitment, most urgent first, one line each. A commitment is something a \
person said they will do ('I will', 'I'll bring this to', 'let me') or was asked to do and accepted. \
Ideas, proposals, wishlist items and 'we could' are not tasks. Drop the due date when none \
was said; drop the parentheses when the owner is also unclear. Add one indented line of context only if \
the task is unclear without it. Omit the section if nobody committed to anything; never write \
'none' or a placeholder.
- Decisions: only what was settled, not ideas floated. Deferrals and rejections count ('X: moved to a \
later iteration', 'no X for now'). Omit the section if nothing was settled.
- Topics: at most 5, in the order discussed; merge small ones. At most 5 bullets each, one level of \
nesting at most.
- Bullets are fragments of at most 15 words: numbers, names, owners, dates. No filler, no restating, \
no 'it was discussed that'.
- Write {ME} for `me`. Owners are a single first name. Never use 'them', 'me', 'speaker', 'team', \
'everyone', 'joint effort' or any other placeholder or group as an owner.
- `them` mixes several voices. Credit a `them` line to a named person only when the transcript ties it \
to them (addressed by name right before, or says their name); otherwise leave the owner out. Someone \
mentioned in a line ('then Anna releases it') is not its speaker. A wrong owner is worse than none.
- An owner is always a person in the meeting: {ME}, someone invited, or someone the transcript shows \
speaking. Never a company, product or glossary term.
- Turn relative dates ('tomorrow', 'Friday', 'next week') into YYYY-MM-DD from the meeting date, no weekdays.
- Fix obvious misrecognitions using the glossary below and context; keep product and customer names exact.
- Skip small talk and weekend chat. No preamble, no closing summary.

Meeting date: {DATE}{ATTENDEES}{VOCAB}";

// Limitation: whole transcript in one request; add map-reduce over chunks if transcripts outgrow the model context.
pub fn summarize(cfg: &Config, meta: &Meta, transcript: &str) -> Result<String> {
    let vocab = cfg
        .vocab
        .as_ref()
        .map(|v| format!("\nGlossary (spellings only; these terms say nothing about this meeting): {v}"))
        .unwrap_or_default();
    let attendees = if meta.attendees.is_empty() {
        String::new()
    } else {
        format!("\nInvited: {}", meta.attendees.join(", "))
    };
    let date = chrono::DateTime::parse_from_rfc3339(&meta.started_at)
        .map(|t| t.format("%A, %Y-%m-%d").to_string())
        .unwrap_or_else(|_| meta.started_at.clone());
    chat(
        cfg,
        &PROMPT
            .replace("{ME}", &cfg.me)
            .replace("{DATE}", &date)
            .replace("{ATTENDEES}", &attendees)
            .replace("{VOCAB}", &vocab),
        transcript,
    )
}

/// One system + user turn against the configured OpenAI-compatible endpoint.
pub fn chat(cfg: &Config, system: &str, user: &str) -> Result<String> {
    chat_messages(
        cfg,
        json!([
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ]),
    )
}

/// A full message list (system, user, assistant, ...) against the configured endpoint.
pub fn chat_messages(cfg: &Config, messages: Value) -> Result<String> {
    let body = json!({ "model": cfg.llm_model(), "messages": messages });
    let url = cfg.llm_url();
    let mut req = ureq::post(&url);
    if let Some(key) = &cfg.llm_key {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let resp: Value = req
        .send_json(body)
        .with_context(|| format!("calling {url}"))?
        .body_mut()
        .read_json()?;
    let text = resp["choices"][0]["message"]["content"]
        .as_str()
        .context("no content in LLM response")?;
    // A reply of only `---` or blank headings counts as empty too.
    anyhow::ensure!(text.chars().any(char::is_alphanumeric), "empty LLM response");
    Ok(text.trim().to_string() + "\n")
}

/// The first `# ` heading: the title line the prompt asks for.
pub fn title(summary: &str) -> Option<&str> {
    summary.lines().find_map(|l| l.strip_prefix("# ")).map(str::trim)
}
