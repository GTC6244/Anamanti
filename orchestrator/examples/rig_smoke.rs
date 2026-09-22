//! Live streaming smoke test for the rig LLM path against a **real** Ollama, with
//! the optional `internet_search` tool (real DuckDuckGo). This is the manual
//! soak step before flipping `rig` on by default. Requires `--features rig`.
//!
//! Prereqs:
//!   ollama serve            # in another terminal
//!   ollama pull qwen2.5     # or set AMBIENT_OLLAMA_MODEL to one you have
//!
//! Run (no helix needed, faster build):
//!   cargo run --no-default-features --features rig --example rig_smoke -- "what is 2+2?"
//!
//! Exercise tool calling (model decides to search, tool runs, answer streams):
//!   AMBIENT_WEB_SEARCH=1 cargo run --no-default-features --features rig \
//!       --example rig_smoke -- "what happened in the news today?"
//!
//! Env:
//!   AMBIENT_OLLAMA_URL    default http://127.0.0.1:11434
//!   AMBIENT_OLLAMA_MODEL  default qwen2.5
//!   AMBIENT_WEB_SEARCH=1  enable the internet_search tool (real web request)

use std::io::Write;
use std::time::Instant;

use std::sync::Arc;

use ambient_orchestrator::directions::LiveHomeLocation;
use ambient_orchestrator::llm::rig::{tools_from_config, RigBackend, Tools};
use ambient_orchestrator::llm::{LlmBackend, LlmTurn};
use futures_util::StreamExt;

/// Minimal tool set for the smoke test: just the (optional) keyless DuckDuckGo web
/// search. Calendar / directions / Spotify are orchestrator features, not exercised
/// here.
fn smoke_tools(web_search: bool) -> Option<Arc<Tools>> {
    tools_from_config(
        web_search,
        "duckduckgo",
        None,
        LiveHomeLocation::default(),
        None,
        None,
        None,
    )
}

const SYSTEM_PROMPT: &str =
    "You are a friendly, concise voice assistant. Answer in one or two short spoken \
     sentences. Do not use markdown, lists, or emoji.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Show `rig tool call: ...` lines when RUST_LOG is set (default: quiet).
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let web_search = matches!(
        std::env::var("AMBIENT_WEB_SEARCH")
            .unwrap_or_default()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "on" | "yes"
    );
    let backend_kind = std::env::var("AMBIENT_LLM_BACKEND")
        .unwrap_or_else(|_| "ollama".into())
        .to_lowercase();

    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.trim().is_empty() {
        "Say hello in one short sentence.".to_string()
    } else {
        prompt
    };

    // Build the selected rig backend. Anthropic needs a key + an explicit model id
    // (we never guess one).
    let backend: Arc<dyn LlmBackend> = match backend_kind.as_str() {
        "anthropic" | "claude" => {
            let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                anyhow::anyhow!("AMBIENT_LLM_BACKEND=anthropic needs ANTHROPIC_API_KEY")
            })?;
            let model = std::env::var("AMBIENT_ANTHROPIC_MODEL").map_err(|_| {
                anyhow::anyhow!(
                    "set AMBIENT_ANTHROPIC_MODEL to a valid model id (e.g. a current Claude model)"
                )
            })?;
            let base = std::env::var("AMBIENT_ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".into());
            let max_tokens: u32 = std::env::var("AMBIENT_ANTHROPIC_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256);
            eprintln!("[rig_smoke] anthropic model={model} web_search={web_search}");
            Arc::new(RigBackend::anthropic(
                &base,
                &key,
                &model,
                max_tokens,
                smoke_tools(web_search),
            )?)
        }
        _ => {
            let url = std::env::var("AMBIENT_OLLAMA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".into());
            let model = std::env::var("AMBIENT_OLLAMA_MODEL").unwrap_or_else(|_| "qwen2.5".into());
            eprintln!("[rig_smoke] ollama={url} model={model} web_search={web_search}");
            Arc::new(RigBackend::ollama(
                &url,
                &model,
                smoke_tools(web_search),
            )?)
        }
    };

    eprintln!("[rig_smoke] prompt: {prompt}");
    eprintln!("[rig_smoke] --- streamed reply below ---");

    let started = Instant::now();
    let mut stream = backend.respond(LlmTurn::new(SYSTEM_PROMPT, prompt)).await?;

    let mut stdout = std::io::stdout();
    let mut first_token_at: Option<Instant> = None;
    let mut tokens = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if first_token_at.is_none() {
            first_token_at = Some(Instant::now());
        }
        tokens += 1;
        print!("{chunk}");
        stdout.flush().ok();
    }
    println!();

    match first_token_at {
        Some(t) => eprintln!(
            "[rig_smoke] {tokens} chunks; time-to-first-token {} ms; total {} ms",
            t.duration_since(started).as_millis(),
            started.elapsed().as_millis(),
        ),
        None => eprintln!(
            "[rig_smoke] no tokens streamed (check the model/name and that Ollama is running)"
        ),
    }
    Ok(())
}
