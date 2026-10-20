//! Meeting notes from a transcript via any OpenAI-compatible chat endpoint.

use crate::{config::Config, store::Meta};
use anyhow::{Context, Result};
use serde_json::{Value, json};

const PROMPT: &str = "You write meeting notes from a raw transcript.

Transcript lines are `[hh:mm:ss] speaker: text`, one line per turn. `me` is {ME}, who recorded the meeting; `them` is everyone \
else (one or several people; use names when the conversation or the invite list reveals them). In a \
1:1 the other person's lines carry their first name instead of `them`. The transcript is machine \
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
person said they will do ('I will', 'I'll bring this to', 'let me', 'I need to align with X', 'I can \
share Y') or was asked to do and accepted ('Sure', 'we can definitely do this'): the one who accepted \
owns it. List {ME}'s own commitments first; first-person promises by `me` are the easiest to miss. \
Ideas, proposals, wishlist items and 'we could' are not tasks. Drop the due date when none \
was said; drop the parentheses when the owner is also unclear. Add one indented line of context only if \
the task is unclear without it. Omit the section if nobody committed to anything; never write \
'none' or a placeholder.
- Decisions: only what was settled in this meeting, not ideas floated. Deferrals and rejections count \
('X: moved to a later iteration', 'no X for now'). An intention, a need or something still to be \
aligned ('we need a policy', 'X must be decided with Y') is not a decision; if it has an owner it is a \
Next Step. Omit the section if nothing was settled.
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
    let notes = chat(
        cfg,
        &PROMPT
            .replace("{ME}", &cfg.me)
            .replace("{DATE}", &date)
            .replace("{ATTENDEES}", &attendees)
            .replace("{VOCAB}", &vocab),
        transcript,
    )?;
    Ok(tidy(&notes))
}

/// What small models do despite the prompt: Next Steps or Decisions sections with no bullets
/// ("(No decisions were reached …)") are dropped, and made-up due dates like "ongoing" removed.
fn tidy(notes: &str) -> String {
    let mut out: Vec<String> = vec![];
    let mut section: Vec<String> = vec![];
    let flush = |out: &mut Vec<String>, section: &mut Vec<String>| {
        let optional = matches!(section.first().map(|h| h.trim()), Some("# Next Steps" | "# Decisions"));
        // A bullet like "- None" or "- (No decisions were made.)" is still no content.
        let item = |l: &String| {
            l.trim_start().strip_prefix("- ").is_some_and(|t| {
                let t = t.trim().trim_end_matches('.').to_lowercase();
                !(t.is_empty()
                    || t.starts_with('(')
                    || t.starts_with("no ")
                    || ["none", "n/a", "tbd"].contains(&t.as_str()))
            })
        };
        if !(optional && !section.iter().any(item)) {
            out.append(section);
        }
        section.clear();
    };
    for line in notes.lines() {
        if line.starts_with("# ") {
            flush(&mut out, &mut section);
        }
        let mut line = line.to_string();
        for filler in ["ongoing", "TBD", "N/A", "none"] {
            line = line
                .replace(&format!(", {filler})"), ")")
                .replace(&format!(" ({filler})"), "");
        }
        section.push(line);
    }
    flush(&mut out, &mut section);
    out.join("\n") + "\n"
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
    // Notes are extraction, not writing: no sampling, so the same meeting gives the same notes.
    let body = json!({ "model": cfg.llm_model(), "messages": messages, "temperature": 0 });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidy_drops_empty_sections_and_filler_dates() {
        let notes =
            "# T\n\n# Next Steps\n- **Share** alerts (Ada, ongoing)\n\n# Decisions\n- None\n\n# Topic\n- fact\n";
        assert_eq!(
            tidy(notes),
            "# T\n\n# Next Steps\n- **Share** alerts (Ada)\n\n# Topic\n- fact\n"
        );
    }
}
