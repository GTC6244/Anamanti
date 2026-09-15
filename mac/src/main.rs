//! Ambient Smart Display — Mac Mini assistant orchestrator (Plan.MD Phase 4).
//!
//! Runs the "brain": a Wyoming server the Echo Show discovers over mDNS, wiring
//! downstream Whisper (STT) → a pluggable LLM + persistent SQLite memory → Piper
//! (TTS), and streaming the synthesized reply back to the device.
//!
//! Configuration is environment-driven (see `config.rs`); with the defaults it
//! advertises `_wyoming._tcp` on port 10700 and talks to a local Whisper (10300),
//! Piper (10200), and Ollama (11434).

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use ambient_orchestrator::config::Config;
use ambient_orchestrator::discovery::MdnsAdvertiser;
use ambient_orchestrator::memory::MemoryStore;
use ambient_orchestrator::orchestrator::{self, Pipeline, TcpConnector};
use ambient_orchestrator::server;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = Config::from_env().context("loading configuration")?;
    log::info!(
        "starting orchestrator: bind={} stt={} tts={} llm={} db={}",
        config.bind_addr,
        config.stt_addr,
        config.tts_addr,
        config.llm_label(),
        config.db_path.display(),
    );

    let memory = Arc::new(MemoryStore::open(&config.db_path).context("opening memory store")?);
    log::info!("memory store holds {} entries", memory.count()?);

    let llm = config.build_llm().context("initializing LLM backend")?;

    let pipeline = Pipeline::new(
        llm,
        memory,
        config.system_prompt.clone(),
        config.tts_voice.clone(),
        config.turn_timeout,
    );

    let connector: Arc<dyn orchestrator::ServiceConnector> = Arc::new(TcpConnector {
        stt_addr: config.stt_addr,
        tts_addr: config.tts_addr,
    });

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("binding device-facing server to {}", config.bind_addr))?;
    let local = listener.local_addr()?;

    // Advertise over mDNS so the Echo Show discovers us without a hardcoded IP.
    // Held for the process lifetime; unregisters on drop.
    let _mdns = MdnsAdvertiser::advertise(&config.service_name, local.port())
        .context("advertising Wyoming service over mDNS")?;

    log::info!("orchestrator ready on {local}; waiting for the device");

    tokio::select! {
        res = server::serve(listener, pipeline, connector) => {
            res.context("device-facing server stopped")?;
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("shutdown signal received; stopping");
        }
    }

    Ok(())
}
