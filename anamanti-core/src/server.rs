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
use crate::notify::NotificationService;
use crate::orchestrator::{Pipeline, ServiceConnector, TurnEvent, TurnOutcome};
use crate::weather::WeatherService;
use crate::wyoming::protocol::{self, types, AudioFormat};
use crate::wyoming::DynConnection;

/// Accept device connections forever, handling each on its own task. Returns only
/// if the listener itself fails.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    listener: TcpListener,
    pipeline: Pipeline,
    connector: Arc<dyn ServiceConnector>,
    catalog: Arc<ModelCatalog>,
    voices_dir: Option<PathBuf>,
    ducker: Option<Arc<MusicDucker>>,
    notify: Arc<NotificationService>,
    weather: Arc<WeatherService>,
) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        log::info!("device connected: {peer}");
        let pipeline = pipeline.clone();
        let connector = connector.clone();
        let catalog = catalog.clone();
        let voices_dir = voices_dir.clone();
        let ducker = ducker.clone();
        let notify = notify.clone();
        let weather = weather.clone();
        tokio::spawn(async move {
            match handle_connection(
                stream, pipeline, connector, catalog, voices_dir, ducker, notify, weather,
            )
            .await
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
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: TcpStream,
    pipeline: Pipeline,
    connector: Arc<dyn ServiceConnector>,
    catalog: Arc<ModelCatalog>,
    voices_dir: Option<PathBuf>,
    ducker: Option<Arc<MusicDucker>>,
    notify: Arc<NotificationService>,
    weather: Arc<WeatherService>,
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
            // device → orchestrator: open the persistent proactive-notification
            // channel (Approach A). We register it and then hold the socket open,
            // pushing `ambient-notify` frames down it as they are enqueued. This is a
            // long-lived connection, separate from a per-turn voice socket, so it
            // takes over this task until the device closes it.
            Some(ev) if ev.event_type == types::ANAMANTI_HELLO => {
                let device_id = ev.hello_device_id().unwrap_or_default().to_string();
                // The persistent channel serves either proactive notifications
                // (`role=notify`, the default) or the ambient weather push
                // (`role=weather`); the write pump below is identical for both — it
                // just drains whichever service's receiver.
                let is_weather = ev.hello_role() == "weather";
                let channel = if is_weather { "weather" } else { "notify" };
                log::info!(
                    "[{}] {channel} channel opened (device_id={device_id:?})",
                    peer_str(peer.as_ref())
                );
                let (conn_id, mut rx) = if is_weather {
                    weather.register(&device_id)
                } else {
                    notify.register(&device_id)
                };
                let (reader, writer) = device.split_mut();
                // Pump enqueued pushes out to the device while watching the read half
                // for the device closing the socket. In this visual-only phase the
                // device sends no frames on this channel, so the read side only ever
                // resolves to EOF/close; a future ack phase that expects inbound
                // frames must move the write pump to a spawned task, because
                // `read_event` is not cancellation-safe mid-frame.
                loop {
                    tokio::select! {
                        incoming = protocol::read_event(&mut *reader) => {
                            match incoming {
                                Ok(Some(_)) => {} // no acks handled yet — ignore
                                Ok(None) | Err(_) => break, // device closed / errored
                            }
                        }
                        outgoing = rx.recv() => {
                            match outgoing {
                                Some(frame) => {
                                    if protocol::write_event(&mut *writer, &frame).await.is_err() {
                                        break;
                                    }
                                }
                                None => break, // notification service dropped
                            }
                        }
                    }
                }
                if is_weather {
                    weather.deregister(conn_id);
                } else {
                    notify.deregister(conn_id);
                }
                log::info!("[{}] {channel} channel closed", peer_str(peer.as_ref()));
                return Ok(());
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
