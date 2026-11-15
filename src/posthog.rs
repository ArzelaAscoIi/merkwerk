//! Process-wide PostHog client initialization.
//!
//! The client is optional so a developer can run merkwerk without analytics
//! configuration. Call sites obtain the single shared instance from
//! `AppState::posthog`.

use std::{
    env,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use posthog_rs::{Client, ClientOptions, Event};
use tokio::runtime::{Builder, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static AI_TRACE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("creating PostHog runtime")
    })
}

/// Builds the one PostHog client when the user opted in and both env vars are set.
/// MCP runs always write configuration warnings to stderr because stdout carries
/// the JSON-RPC protocol.
pub fn init(is_mcp: bool, enabled: bool) -> Option<Arc<Client>> {
    if !enabled {
        return None;
    }
    let token = env::var("POSTHOG_PROJECT_TOKEN").ok().filter(|value| !value.trim().is_empty());
    let host = env::var("POSTHOG_HOST").ok().filter(|value| !value.trim().is_empty());
    let should_warn = cfg!(debug_assertions) || is_mcp;

    if token.is_none() && should_warn {
        eprintln!("POSTHOG_PROJECT_TOKEN variable required by PostHog is missing or un-configured, this causes events to be silently missed. This error stops appearing once POSTHOG_PROJECT_TOKEN is configured");
    }
    if host.is_none() && should_warn {
        eprintln!("POSTHOG_HOST variable required by PostHog is missing or un-configured, this causes events to be silently missed. This error stops appearing once POSTHOG_HOST is configured");
    }

    let (Some(token), Some(host)) = (token, host) else {
        return None;
    };
    let options = ClientOptions::from((token.as_str(), host.as_str()));
    Some(Arc::new(runtime().block_on(posthog_rs::client(options))))
}

/// Queues a personless product event for this local-only application. Capture is
/// fire-and-forget, so analytics can never block the action that triggered it.
pub fn capture(client: Option<Arc<Client>>, event: &str) {
    if let Some(client) = client {
        client.capture(Event::new_anon(event));
    }
}

/// Queues one OpenAI-compatible generation for AI Observability. This is
/// fire-and-forget, so an unavailable PostHog service cannot delay the LLM call.
pub fn capture_ai_generation(
    client: Option<Arc<Client>>,
    session_id: Option<&str>,
    span_name: &str,
    model: &str,
    provider: &str,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    latency: Duration,
    is_error: bool,
) {
    let Some(client) = client else {
        return;
    };
    let trace_id = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        AI_TRACE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    );
    let mut event = Event::new_anon("$ai_generation");
    let _ = event.insert_prop("$ai_trace_id", trace_id);
    let _ = event.insert_prop("$ai_session_id", session_id);
    let _ = event.insert_prop("$ai_span_name", span_name);
    let _ = event.insert_prop("$ai_model", model);
    let _ = event.insert_prop("$ai_provider", provider);
    let _ = event.insert_prop("$ai_latency", latency.as_secs_f64());
    if let Some(tokens) = input_tokens {
        let _ = event.insert_prop("$ai_input_tokens", tokens);
    }
    if let Some(tokens) = output_tokens {
        let _ = event.insert_prop("$ai_output_tokens", tokens);
    }
    if is_error {
        let _ = event.insert_prop("$ai_is_error", true);
    }
    client.capture(event);
}

/// Reports an error that escaped the application boundary. Capture is best-effort
/// so telemetry failures never affect the command's original result.
pub fn capture_exception(client: Option<Arc<Client>>, error: &(dyn std::error::Error + 'static)) {
    let Some(client) = client else {
        return;
    };
    if let Err(capture_error) = runtime().block_on(client.capture_exception(error)) {
        eprintln!("PostHog exception capture failed: {capture_error}");
    }
}

/// Sends queued events before a command exits; errors never interrupt the app.
pub fn shutdown(client: Option<Arc<Client>>) {
    if let Some(client) = client {
        runtime().block_on(async {
            client.flush().await;
            client.shutdown().await;
        });
    }
}
