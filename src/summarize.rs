//! Meeting notes from a transcript via any OpenAI-compatible chat endpoint.

use crate::config::Config;
use anyhow::{Context, Result};
use serde_json::{Value, json};

const PROMPT: &str = "You write meeting notes from a raw transcript.

Transcript lines are `[hh:mm:ss] speaker: text`. `me` is {ME}, who recorded the meeting; `them` is everyone \
else (one or several people; use names when the conversation reveals them). The transcript is machine \
generated and can be German, English or mixed, with recognition errors: infer intent, never invent facts.

Always write the notes in English, in Markdown, in this shape:

# <short descriptive title for the whole meeting>

# <Topic>
- terse bullets with the concrete facts: numbers, names, decisions, open questions
  - nested bullets for detail or examples

(one `# <Topic>` section per topic discussed, in the order discussed)

# Next Steps
- **<task>** (<owner first name>)

  <one line of context, including any deadline>

Rules:
- Only `# ` headings, no `##`.
- Write {ME} for `me`. The owner is a single first name in parentheses; omit the parentheses entirely \
when the owner is unclear. Never use 'them' or 'me' as an owner.
- Fix obvious misrecognitions using the glossary below and context; keep product and customer names exact.
- Skip small talk. No preamble.{VOCAB}";

// Limitation: whole transcript in one request; add map-reduce over chunks if transcripts outgrow the model context.
pub fn summarize(cfg: &Config, transcript: &str) -> Result<String> {
    let vocab = cfg
        .vocab
        .as_ref()
        .map(|v| format!("\n\nGlossary: {v}"))
        .unwrap_or_default();
    chat(
        cfg,
        &PROMPT.replace("{ME}", &cfg.me).replace("{VOCAB}", &vocab),
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
    Ok(text.trim().to_string() + "\n")
}

/// The first `# ` heading: the title line the prompt asks for.
pub fn title(summary: &str) -> Option<&str> {
    summary.lines().find_map(|l| l.strip_prefix("# ")).map(str::trim)
}
