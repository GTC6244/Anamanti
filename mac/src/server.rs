//! The device-facing Wyoming **server**: accepts the Echo Show's TCP connection
//! and drives turns through the [`Pipeline`]. This is the single endpoint the
//! device discovers over mDNS (Plan.MD Phase 4 bullet 5, "frames stream back to
//! the Echo Show over the Wyoming socket").

use std::sync::Arc;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::orchestrator::{Pipeline, ServiceConnector, TurnEvent, TurnOutcome};
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

/// Handle one device connection: run turns until the device closes the socket.
/// The Phase-3 device opens a fresh connection per turn, but looping here also
/// supports a Phase-5 device that reuses one socket for several turns.
async fn handle_connection(
    stream: TcpStream,
    pipeline: Pipeline,
    connector: Arc<dyn ServiceConnector>,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    let mut device = DynConnection::from_tcp_stream(stream);

    loop {
        let mut on_event = |ev: TurnEvent| log_event(peer.as_ref(), &ev);
        match pipeline
            .run_turn(&mut device, connector.as_ref(), &mut on_event)
            .await?
        {
            TurnOutcome::Completed => continue,
            TurnOutcome::Disconnected => return Ok(()),
        }
    }
}

fn log_event(peer: Option<&std::net::SocketAddr>, ev: &TurnEvent) {
    let who = peer.map(|p| p.to_string()).unwrap_or_default();
    match ev {
        TurnEvent::Streaming => log::debug!("[{who}] streaming audio to STT"),
        TurnEvent::Transcript(t) => log::info!("[{who}] transcript: {t:?}"),
        TurnEvent::MemoryStored(c) => log::info!("[{who}] remembered: {c:?}"),
        TurnEvent::Reply(r) => log::info!("[{who}] reply: {r:?}"),
        TurnEvent::Speaking => log::debug!("[{who}] streaming TTS audio to device"),
        TurnEvent::Finished => log::debug!("[{who}] turn finished"),
        TurnEvent::ReplyToken(_) => {}
    }
}
