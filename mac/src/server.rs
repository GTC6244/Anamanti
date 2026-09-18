//! The device-facing Wyoming **server**: accepts the Echo Show's TCP connection
//! and drives turns through the [`Pipeline`]. This is the single endpoint the
//! device discovers over mDNS (Plan.MD Phase 4 bullet 5, "frames stream back to
//! the Echo Show over the Wyoming socket").

use std::sync::Arc;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::control;
use crate::orchestrator::{Pipeline, ServiceConnector, TurnEvent, TurnOutcome};
use crate::wyoming::protocol::{self, types, AudioFormat};
use crate::wyoming::DynConnection;

/// Accept device connections forever, handling each on its own task. Returns only
/// if the listener itself fails.
pub async fn serve(
    listener: TcpListener,
    pipeline: Pipeline,
    connector: Arc<dyn ServiceConnector>,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        log::info!("device connected: {peer}");
        let pipeline = pipeline.clone();
        let connector = connector.clone();
        tokio::spawn(async move {
            match handle_connection(stream, pipeline, connector).await {
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
                )
                .await?;
            }
            Some(ev) if ev.event_type == types::AUDIO_START => {
                let format = protocol::audio_format(&ev.data).unwrap_or(AudioFormat::PCM_16K_MONO);
                let mut on_event = |ev: TurnEvent| log_event(peer.as_ref(), &ev);
                match pipeline
                    .run_turn_after_start(&mut device, connector.as_ref(), format, &mut on_event)
                    .await?
                {
                    TurnOutcome::Completed => continue,
                    TurnOutcome::Disconnected => return Ok(()),
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
