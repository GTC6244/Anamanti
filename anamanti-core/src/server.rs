//! The device-facing Wyoming **server**: accepts the Echo Show's TCP connection
//! and drives turns through the [`Pipeline`]. This is the single endpoint the
//! device discovers over mDNS (Plan.MD Phase 4 bullet 5, "frames stream back to
//! the Echo Show over the Wyoming socket").

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::app::AppServices;
use crate::control;
use crate::music::{MusicDucker, NowPlayingService};
use crate::notify::NotificationService;
use crate::orchestrator::{Pipeline, ServiceConnector, TurnEvent, TurnOutcome, TurnStart};
use crate::weather::WeatherService;
use crate::wyoming::protocol::{self, types};
use crate::wyoming::DynConnection;

/// Lowers and restores the house music around a spoken reply. Implemented by
/// [`MusicDucker`] (Snapcast); abstracted so the turn wiring is testable.
#[async_trait]
pub trait Ducker: Send + Sync {
    async fn duck(&self) -> Result<()>;
    async fn restore(&self) -> Result<()>;
}

#[async_trait]
impl Ducker for MusicDucker {
    async fn duck(&self) -> Result<()> {
        MusicDucker::duck(self).await
    }
    async fn restore(&self) -> Result<()> {
        MusicDucker::restore(self).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DuckOp {
    Duck,
    Restore,
}

/// Music ducking for one device connection. Duck/restore requests go down one channel
/// to one task that applies them **in order**, so a restore can never overtake the duck
/// it undoes (two independently spawned tasks could race). Best-effort: a failure is
/// logged and never touches the turn. The task ends once this handle (and every
/// [`TurnDuck`]) is dropped and the queue has drained.
pub struct ConnectionDucking {
    tx: mpsc::UnboundedSender<DuckOp>,
}

impl ConnectionDucking {
    /// Start the per-connection ducking task.
    pub fn spawn(ducker: Arc<dyn Ducker>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<DuckOp>();
        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                let res = match op {
                    DuckOp::Duck => ducker.duck().await,
                    DuckOp::Restore => ducker.restore().await,
                };
                if let Err(e) = res {
                    log::debug!("music {op:?} failed: {e:#}");
                }
            }
        });
        Self { tx }
    }

    /// A guard for one turn: [`TurnDuck::observe`] ducks on the first
    /// [`TurnEvent::Speaking`], and dropping the guard restores — on every exit path
    /// (a finished reply, an error, a timeout, a barge-in, a disconnect, a panic).
    pub fn turn(&self) -> TurnDuck {
        TurnDuck {
            tx: self.tx.clone(),
            ducked: AtomicBool::new(false),
        }
    }
}

/// Per-turn ducking guard (see [`ConnectionDucking::turn`]).
pub struct TurnDuck {
    tx: mpsc::UnboundedSender<DuckOp>,
    ducked: AtomicBool,
}

impl TurnDuck {
    /// Feed a turn event: the first `Speaking` ducks the music.
    pub fn observe(&self, ev: &TurnEvent) {
        if matches!(ev, TurnEvent::Speaking) && !self.ducked.swap(true, Ordering::SeqCst) {
            let _ = self.tx.send(DuckOp::Duck);
        }
    }
}

impl Drop for TurnDuck {
    fn drop(&mut self) {
        if *self.ducked.get_mut() {
            let _ = self.tx.send(DuckOp::Restore);
        }
    }
}

/// Run one voice turn on `device` after its opening `audio-start` has been parsed into
/// `start` — the entry point the connection handler uses (and the pipeline tests
/// drive). Music is ducked while the reply speaks and restored when the turn ends,
/// however it ends; `on_event` sees every [`TurnEvent`].
pub async fn drive_turn(
    pipeline: &Pipeline,
    device: &mut DynConnection,
    connector: &dyn ServiceConnector,
    start: TurnStart,
    ducking: Option<&ConnectionDucking>,
    on_event: &mut (dyn FnMut(TurnEvent) + Send),
) -> Result<TurnOutcome> {
    let duck = ducking.map(ConnectionDucking::turn);
    let mut observe = |ev: TurnEvent| {
        if let Some(d) = duck.as_ref() {
            d.observe(&ev);
        }
        on_event(ev);
    };
    pipeline
        .run_turn_after_start(device, connector, start, &mut observe)
        .await
    // `duck` drops here (or on unwind), restoring the music if this turn ducked it.
}

/// Accept device connections forever, handling each on its own task. Returns only
/// if the listener itself fails.
pub async fn serve(listener: TcpListener, services: Arc<AppServices>) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        log::info!("device connected: {peer}");
        let services = services.clone();
        tokio::spawn(async move {
            match handle_connection(stream, &services).await {
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
async fn handle_connection(stream: TcpStream, services: &AppServices) -> Result<()> {
    let pipeline = &services.pipeline;
    let connector = services.connector.as_ref();
    let peer = stream.peer_addr().ok();
    let mut device = DynConnection::from_tcp_stream(stream);
    // Music ducking for this connection's turns, applied in order on one task. Started
    // on the first turn, so control-only and push-channel connections never spawn it.
    let mut ducking: Option<ConnectionDucking> = None;

    loop {
        match device.read().await? {
            None => return Ok(()), // clean close
            Some(ev) if control::is_control_request(&ev.event_type) => {
                log::info!(
                    "[{}] control request: {}",
                    peer_str(peer.as_ref()),
                    ev.event_type
                );
                control::handle_control(&mut device, &ev, services).await?;
            }
            Some(ev) if ev.event_type == types::AUDIO_START => {
                // The opening frame carries the PCM format, the follow-up chain depth +
                // listen window, and the device context (what the display shows, plus
                // background state like running timers).
                let Some(start) = TurnStart::from_event(&ev) else {
                    continue;
                };
                if ducking.is_none() {
                    ducking = services.ducker.clone().map(ConnectionDucking::spawn);
                }
                let mut on_event = |ev: TurnEvent| log_event(peer.as_ref(), &ev);
                match drive_turn(
                    pipeline,
                    &mut device,
                    connector,
                    start,
                    ducking.as_ref(),
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
                    pipeline.announce(&mut device, connector, &text).await?;
                }
            }
            // device → orchestrator: open the persistent proactive-notification
            // channel (Approach A). We register it and then hold the socket open,
            // pushing `ambient-notify` frames down it as they are enqueued. This is a
            // long-lived connection, separate from a per-turn voice socket, so it
            // takes over this task until the device closes it.
            Some(ev) if ev.event_type == types::ANAMANTI_HELLO => {
                let device_id = ev.hello_device_id().unwrap_or_default().to_string();
                let device_name = ev.hello_name().unwrap_or_default().to_string();
                // The persistent channel serves proactive notifications (`role=notify`,
                // the default), the ambient weather push (`role=weather`), or the music
                // now-playing push (`role=music`); the write pump below is identical for
                // all three — it just drains whichever service's receiver.
                let channel = PushChannel::for_role(ev.hello_role(), services);
                log::info!(
                    "[{}] {} channel opened (device_id={device_id:?}, name={device_name:?})",
                    peer_str(peer.as_ref()),
                    channel.label(),
                );
                let (conn_id, mut rx) = channel.register(&device_id, &device_name);
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
                channel.deregister(conn_id);
                log::info!(
                    "[{}] {} channel closed",
                    peer_str(peer.as_ref()),
                    channel.label()
                );
                return Ok(());
            }
            Some(_) => continue, // ignore stray pre-turn frames
        }
    }
}

/// The persistent push service an `anamanti-hello` frame opens, looked up by its
/// `role`: `weather` → the ambient weather push, `music` → the now-playing push,
/// anything else (the default `notify`) → proactive notifications.
enum PushChannel<'a> {
    Notify(&'a NotificationService),
    Weather(&'a WeatherService),
    Music(&'a NowPlayingService),
}

impl<'a> PushChannel<'a> {
    fn for_role(role: &str, services: &'a AppServices) -> Self {
        match role {
            "weather" => PushChannel::Weather(&services.weather),
            "music" => PushChannel::Music(&services.now_playing),
            _ => PushChannel::Notify(&services.notify),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            PushChannel::Notify(_) => "notify",
            PushChannel::Weather(_) => "weather",
            PushChannel::Music(_) => "music",
        }
    }

    /// Register the channel. Notify channels record the device identity for the config
    /// page's connected-devices list; music channels record it so each display's push
    /// carries its own music widget style.
    fn register(
        &self,
        device_id: &str,
        name: &str,
    ) -> (
        u64,
        tokio::sync::mpsc::UnboundedReceiver<protocol::WyomingEvent>,
    ) {
        match self {
            PushChannel::Notify(svc) => svc.register(device_id, name),
            PushChannel::Weather(svc) => svc.register(),
            PushChannel::Music(svc) => svc.register(device_id, name),
        }
    }

    fn deregister(&self, conn_id: u64) {
        match self {
            PushChannel::Notify(svc) => svc.deregister(conn_id),
            PushChannel::Weather(svc) => svc.deregister(conn_id),
            PushChannel::Music(svc) => svc.deregister(conn_id),
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
