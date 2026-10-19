use std::{env, path::PathBuf};

/// Everything is configured through env vars; defaults are fully local.
#[derive(Clone)]
pub struct Config {
    /// Where meetings live: one directory per meeting.
    pub root: PathBuf,
    /// Parakeet TDT 0.6B v3 ONNX directory.
    pub parakeet: PathBuf,
    /// whisper.cpp ggml model file.
    pub model: PathBuf,
    /// whisper large-v2 ggml model, next to `model`; slower than turbo, often more accurate.
    pub model_v2: PathBuf,
    /// Silero VAD model (ggml), next to the whisper model; drops non-speech before transcription.
    pub vad: PathBuf,
    /// Whisper language code, comma-separated candidates (picked per utterance), or "auto".
    pub lang: String,
    /// Names and jargon from `$MERKWERK_DIR/vocab.txt` (one term per line), as "a, b, c".
    /// Used by the notes step to fix misrecognitions; whisper itself gets no prompt, because a
    /// glossary there biases it ("Okay" became "OKR") and the transcript should stay raw.
    pub vocab: Option<String>,
    pub llm_key: Option<String>,
    /// How the notes name the person recording (`me` in transcripts).
    pub me: String,
    /// Process name fragments whose mic usage is not a call: other recorders, dictation tools.
    pub ignore: Vec<String>,
    /// App, bundle id or window-title fragments never captured; defaults plus `MERKWERK_SCREEN_IGNORE`.
    pub screen_ignore: Vec<String>,
}

pub const PARAKEET_URL: &str = "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main";
/// int8 exports: about a quarter of the fp32 size at near-identical accuracy.
pub const PARAKEET_FILES: [&str; 3] = ["encoder-model.int8.onnx", "decoder_joint-model.int8.onnx", "vocab.txt"];
pub const VAD_URL: &str = "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin";
pub const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
pub const MODEL_V2_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v2-q5_0.bin";
/// Notes servers offered in the menu: (label, chat completions URL). The first is the default.
pub const LLM_SERVERS: [(&str, &str); 2] = [
    ("Ollama", "http://localhost:11434/v1/chat/completions"),
    ("LM Studio", "http://localhost:1234/v1/chat/completions"),
];

impl Config {
    pub fn from_env() -> Self {
        let var = |k: &str| env::var(k).ok().filter(|v| !v.is_empty());
        let root = var("MERKWERK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env::var("HOME").expect("HOME not set")).join("merkwerk"));
        let model = var("MERKWERK_MODEL")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("models/ggml-large-v3-turbo-q5_0.bin"));
        Config {
            vocab: std::fs::read_to_string(root.join("vocab.txt"))
                .ok()
                .map(|v| {
                    v.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|v| !v.is_empty()),
            vad: model.with_file_name("ggml-silero-v5.1.2.bin"),
            parakeet: model.with_file_name("parakeet-tdt-0.6b-v3"),
            model_v2: model.with_file_name("ggml-large-v2-q5_0.bin"),
            model,
            lang: var("MERKWERK_LANG").unwrap_or_else(|| "en,de".into()),
            llm_key: var("MERKWERK_LLM_KEY"),
            me: var("MERKWERK_ME").unwrap_or_else(first_name),
            ignore: list(&var("MERKWERK_IGNORE").unwrap_or_else(|| "merkwerk,granola,screenpipe,openwhispr".into())),
            screen_ignore: list(&format!(
                "{},{}",
                crate::screen::DEFAULT_IGNORE,
                var("MERKWERK_SCREEN_IGNORE").unwrap_or_default()
            )),
            root,
        }
    }
}

/// Comma-separated, trimmed, lowercased, empties dropped.
fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// First name from the macOS account, e.g. "Kristof".
fn first_name() -> String {
    std::process::Command::new("id")
        .arg("-F")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|n| n.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "me".into())
}

impl Config {
    /// Speech-to-text engine, `parakeet`, `whisper` or `whisper-v2`: env var, else the menu choice, else parakeet.
    pub fn engine(&self) -> String {
        chosen(&self.root, "MERKWERK_ENGINE", "engine", "parakeet")
    }

    /// LLM for notes and work logs: env var, else the menu choice, else gemma4.
    pub fn llm_model(&self) -> String {
        chosen(&self.root, "MERKWERK_LLM_MODEL", "llm_model", "gemma4")
    }

    /// Chat completions endpoint for notes: env var, else the menu choice, else Ollama.
    /// Any OpenAI-compatible endpoint works (Ollama, LM Studio, OpenAI, OpenRouter...).
    pub fn llm_url(&self) -> String {
        chosen(&self.root, "MERKWERK_LLM_URL", "llm_url", LLM_SERVERS[0].1)
    }

    /// Screen activity on start: `MERKWERK_SCREEN` (0 or 1), else the menu choice, else on.
    pub fn screen_enabled(&self) -> bool {
        match std::env::var("MERKWERK_SCREEN").ok().as_deref() {
            Some("0") => false,
            Some(_) => true,
            None => setting(&self.root, "screen").and_then(|v| v.as_bool()).unwrap_or(true),
        }
    }

    /// OpenAI-compatible model list (`GET /v1/models`) of the notes server.
    pub fn models_url(&self) -> Option<String> {
        let url = self.llm_url();
        let (base, _) = url.split_once("/v1/")?;
        Some(format!("{base}/v1/models"))
    }
}

fn chosen(root: &std::path::Path, env: &str, key: &str, default: &str) -> String {
    std::env::var(env)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| setting(root, key).and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_else(|| default.into())
}

/// `settings.json`: choices made in the app (recording mode, models).
pub fn setting(root: &std::path::Path, key: &str) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(root.join("settings.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw).ok()?.get(key).cloned()
}

pub fn set_setting(root: &std::path::Path, key: &str, value: serde_json::Value) -> std::io::Result<()> {
    let path = root.join("settings.json");
    let mut all: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    all.insert(key.into(), value);
    std::fs::create_dir_all(root)?;
    std::fs::write(path, serde_json::to_string_pretty(&all)?)
}

pub fn auto_record(root: &std::path::Path) -> bool {
    setting(root, "auto_record").and_then(|v| v.as_bool()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_choice_overrides_default_and_merges_settings() {
        let root = std::env::temp_dir().join(format!("merkwerk-test-{}", std::process::id()));
        set_setting(&root, "auto_record", true.into()).unwrap();
        set_setting(&root, "llm_model", "gemma4:26b".into()).unwrap();
        assert_eq!(
            chosen(&root, "MERKWERK_TEST_UNSET", "llm_model", "gemma4"),
            "gemma4:26b"
        );
        assert_eq!(chosen(&root, "MERKWERK_TEST_UNSET", "engine", "parakeet"), "parakeet");
        assert!(auto_record(&root), "earlier keys survive later writes");
        let _ = std::fs::remove_dir_all(root);
    }
}
