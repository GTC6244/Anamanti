//! The device-facing Wyoming **server**: accepts the Echo Show's TCP connection
//! and drives turns through the [`Pipeline`]. This is the single endpoint the
//! device discovers over mDNS (Plan.MD Phase 4 bullet 5, "frames stream back to
//! the Echo Show over the Wyoming socket").

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::control;
use crate::llm::catalog::ModelCatalog;
use crate::music::MusicDucker;
use crate::orchestrator::{Pipeline, ServiceConnector, TurnEvent, TurnOutcome};
use crate::wyoming::protocol::{self, types, AudioFormat};
use crate::wyoming::DynConnection;

/// Accept device connections forever, handling each on its own task. Returns only
/// if the listener itself fails.
pub async fn serve(
    listener: TcpListener,
    pipeline: Pipeline,
    connector: Arc<dyn ServiceConnector>,
    catalog: Arc<ModelCatalog>,
    voices_dir: Option<PathBuf>,
    ducker: Option<Arc<MusicDucker>>,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        log::info!("device connected: {peer}");
        let pipeline = pipeline.clone();
        let connector = connector.clone();
        let catalog = catalog.clone();
        let voices_dir = voices_dir.clone();
        let ducker = ducker.clone();
        tokio::spawn(async move {
            match handle_connection(stream, pipeline, connector, catalog, voices_dir, ducker).await
            {
                Ok(()) => log::info!("device disconnected: {peer}"),
                Err(e) => log::warn!("connection {peer} ended with error: {e:#}"),
            }
        });
    }
}

/// Handle one device connection until the device closes the socket. Each inbound
/// frame is routed by type: a Phase-6 `ambient-*` control frame is answered from
/// the memory store / runtime settings; an `audio-start` opens a voice turn. The
/// Phase-3 device opens a fresh connection per turn, but looping here also supports
/// a device that reuses one socket for several turns or control requests.
async fn handle_connection(
    stream: TcpStream,
    pipeline: Pipeline,
    connector: Arc<dyn ServiceConnector>,
    catalog: Arc<ModelCatalog>,
    voices_dir: Option<PathBuf>,
    ducker: Option<Arc<MusicDucker>>,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    let mut device = DynConnection::from_tcp_stream(stream);

    loop {
        match device.read().await? {
            None => return Ok(()), // clean close
            Some(ev) if control::is_control_request(&ev.event_type) => {
                log::info!(
                    "[{}] control request: {}",
                    peer_str(peer.as_ref()),
                    ev.event_type
                );
                control::handle_control(
                    &mut device,
                    &ev,
                    pipeline.memory(),
                    pipeline.settings(),
                    pipeline.speaker().map(|s| s.registry()),
                    &catalog,
                    connector.as_ref(),
                    voices_dir.as_deref(),
                )
                .await?;
            }
            Some(ev) if ev.event_type == types::AUDIO_START => {
                let format = protocol::audio_format(&ev.data).unwrap_or(AudioFormat::PCM_16K_MONO);
                // A follow-up turn (device auto-opened the mic) stamps its chain depth
                // and the listen window it was given; an ordinary wake-word turn reads
                // both back as 0.
                let followup_depth = protocol::followup_depth(&ev.data);
                let followup_wait_secs = protocol::followup_wait_secs(&ev.data);
                // Best-effort music ducking: lower the music group's volume while
                // the assistant speaks and restore it when the turn ends. Fired on
                // a spawned task so it never blocks (or fails) the turn; `duck`/
                // `restore` are idempotent, so repeated `Speaking` events are safe.
                let mut on_event = |ev: TurnEvent| {
                    log_event(peer.as_ref(), &ev);
                    if let Some(d) = ducker.as_ref() {
                        match &ev {
                            TurnEvent::Speaking => {
                                let d = d.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = d.duck().await {
                                        log::debug!("music duck failed: {e:#}");
                                    }
                                });
                            }
                            TurnEvent::Finished => {
                                let d = d.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = d.restore().await {
                                        log::debug!("music restore failed: {e:#}");
                                    }
                                });
                            }
                            _ => {}
                        }
                    }
                };
                match pipeline
                    .run_turn_after_start(
                        &mut device,
                        connector.as_ref(),
                        format,
                        followup_depth,
                        followup_wait_secs,
                        &mut on_event,
                    )
                    .await?
                {
                    TurnOutcome::Completed => continue,
                    TurnOutcome::Disconnected => return Ok(()),
                }
            }
            // device → orchestrator: synthesize an out-of-band announcement (e.g. a
            // timer's "Time's up …") in the Piper voice and stream it straight back.
            Some(ev) if ev.event_type == types::SPEAK => {
                let text = ev.speak_text().unwrap_or_default().to_string();
                log::info!("[{}] speak request: {text:?}", peer_str(peer.as_ref()));
                if !text.trim().is_empty() {
                    pipeline
                        .announce(&mut device, connector.as_ref(), &text)
                        .await?;
                }
            }
            Some(_) => continue, // ignore stray pre-turn frames
        }
    }
}

fn peer_str(peer: Option<&std::net::SocketAddr>) -> String {
    peer.map(|p| p.to_string()).unwrap_or_default()
}

fn log_event(peer: Option<&std::net::SocketAddr>, ev: &TurnEvent) {
    let who = peer.map(|p| p.to_string()).unwrap_or_default();
    match ev {
        TurnEvent::Streaming => log::debug!("[{who}] streaming audio to STT"),
        TurnEvent::Transcript(t) => log::info!("[{who}] transcript: {t:?}"),
        TurnEvent::Speaker(s) => {
            let label = s.name.as_deref().unwrap_or(&s.speaker_id);
            log::info!(
                "[{who}] speaker: {label}{} (confidence {:.2})",
                if s.is_new { " [new]" } else { "" },
                s.confidence
            );
        }
        TurnEvent::MemoryStored(c) => log::info!("[{who}] remembered: {c:?}"),
        TurnEvent::Reply(r) => log::info!("[{who}] reply: {r:?}"),
        TurnEvent::Speaking => log::debug!("[{who}] streaming TTS audio to device"),
        TurnEvent::Finished => log::debug!("[{who}] turn finished"),
        TurnEvent::ReplyToken(_) => {}
    }
}
