//! Process supervisor for the music sibling processes (snapserver, librespot,
//! mpv web player) + a [`MusicHub`] that bundles everything the config-page
//! Music tab needs (`webconfig.rs`).
//!
//! These are the same commands the runbook / launchd agents run
//! (`orchestrator/deploy/snapcast/`), but driven from the orchestrator so you can
//! start/stop them and see status from the browser instead of a terminal. The
//! orchestrator still never touches the audio PCM — it only launches the
//! processes and talks control protocols. Children are spawned with
//! `kill_on_drop(true)`, so they are cleaned up when the orchestrator exits.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use super::{MpvControl, SnapcastClient};

/// The externally-launched processes the orchestrator can supervise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagedProc {
    /// The Snapcast server (sync hub + JSON-RPC control).
    Snapserver,
    /// librespot — the Spotify Connect device feeding the Spotify snapfifo.
    Librespot,
    /// mpv — the web-URL player feeding the Web snapfifo, driven over JSON IPC.
    MpvWeb,
}

impl ManagedProc {
    /// Stable url/JSON key.
    pub fn key(self) -> &'static str {
        match self {
            ManagedProc::Snapserver => "snapserver",
            ManagedProc::Librespot => "librespot",
            ManagedProc::MpvWeb => "mpv-web",
        }
    }

    /// Human label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            ManagedProc::Snapserver => "Snapserver (sync hub)",
            ManagedProc::Librespot => "librespot (Spotify Connect)",
            ManagedProc::MpvWeb => "mpv (web-URL player)",
        }
    }

    /// Parse a url/JSON key back to a proc.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "snapserver" => Some(ManagedProc::Snapserver),
            "librespot" => Some(ManagedProc::Librespot),
            "mpv-web" => Some(ManagedProc::MpvWeb),
            _ => None,
        }
    }

    /// All managed procs in display order.
    pub fn all() -> [ManagedProc; 3] {
        [
            ManagedProc::Snapserver,
            ManagedProc::Librespot,
            ManagedProc::MpvWeb,
        ]
    }
}

/// How to launch one managed process.
#[derive(Debug, Clone)]
pub struct ProcSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub log_path: PathBuf,
}

/// One process's current state, for the status endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct ProcStatus {
    pub key: &'static str,
    pub label: &'static str,
    pub running: bool,
    pub pid: Option<u32>,
    pub program: String,
    pub log_path: String,
}

/// Supervises the music sibling processes: start, stop, and report status.
#[derive(Debug)]
pub struct MusicSupervisor {
    specs: HashMap<ManagedProc, ProcSpec>,
    running: Mutex<HashMap<ManagedProc, Child>>,
}

impl MusicSupervisor {
    pub fn new(specs: HashMap<ManagedProc, ProcSpec>) -> Self {
        Self {
            specs,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// Start `proc`. Errors if it is already running (per this supervisor) or the
    /// program cannot be spawned. Child stdout/stderr append to its log file.
    pub async fn start(&self, proc: ManagedProc) -> Result<()> {
        let spec = self
            .specs
            .get(&proc)
            .with_context(|| format!("no spec for {}", proc.key()))?;
        let mut running = self.running.lock().await;
        if let Some(child) = running.get_mut(&proc) {
            // Prune a child that has already exited so we can restart it.
            if matches!(child.try_wait(), Ok(Some(_))) {
                running.remove(&proc);
            } else {
                bail!("{} is already running", proc.key());
            }
        }
        if let Some(dir) = spec.log_path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log_path)
            .with_context(|| format!("opening log {}", spec.log_path.display()))?;
        let log_err = log.try_clone().context("cloning log handle for stderr")?;
        let child = Command::new(&spec.program)
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {}", spec.program.display()))?;
        log::info!(
            "music: started {} (pid {:?}) → {}",
            proc.key(),
            child.id(),
            spec.log_path.display()
        );
        running.insert(proc, child);
        Ok(())
    }

    /// Stop `proc` if running. No-op (Ok) if it is not tracked as running.
    pub async fn stop(&self, proc: ManagedProc) -> Result<()> {
        let mut running = self.running.lock().await;
        if let Some(mut child) = running.remove(&proc) {
            child
                .kill()
                .await
                .with_context(|| format!("killing {}", proc.key()))?;
            log::info!("music: stopped {}", proc.key());
        }
        Ok(())
    }

    /// Current status of every managed process (pruning any that have exited).
    pub async fn status(&self) -> Vec<ProcStatus> {
        let mut running = self.running.lock().await;
        let mut out = Vec::with_capacity(self.specs.len());
        for proc in ManagedProc::all() {
            let Some(spec) = self.specs.get(&proc) else {
                continue;
            };
            let (is_running, pid) = match running.get_mut(&proc) {
                Some(child) => match child.try_wait() {
                    Ok(Some(_)) => (false, None), // exited
                    _ => (true, child.id()),
                },
                None => (false, None),
            };
            if !is_running {
                running.remove(&proc);
            }
            out.push(ProcStatus {
                key: proc.key(),
                label: proc.label(),
                running: is_running,
                pid,
                program: spec.program.display().to_string(),
                log_path: spec.log_path.display().to_string(),
            });
        }
        out
    }
}

/// Everything the config-page Music tab needs: the process supervisor plus the
/// snapserver control client (for live status) and the mpv control (for the
/// "play URL" box). Cheaply cloneable.
#[derive(Clone)]
pub struct MusicHub {
    pub supervisor: Arc<MusicSupervisor>,
    pub snapcast: SnapcastClient,
    pub snapserver_addr: String,
    pub mpv: Option<MpvControl>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_spec(proc: ManagedProc, spec: ProcSpec) -> MusicSupervisor {
        let mut specs = HashMap::new();
        specs.insert(proc, spec);
        MusicSupervisor::new(specs)
    }

    #[test]
    fn proc_key_roundtrips() {
        for p in ManagedProc::all() {
            assert_eq!(ManagedProc::from_key(p.key()), Some(p));
        }
        assert_eq!(ManagedProc::from_key("nope"), None);
    }

    #[tokio::test]
    async fn start_status_stop_cycle() {
        let log = std::env::temp_dir().join(format!("ambient-sup-{}.log", std::process::id()));
        let sup = one_spec(
            ManagedProc::Snapserver,
            ProcSpec {
                program: PathBuf::from("sleep"),
                args: vec!["30".into()],
                log_path: log.clone(),
            },
        );

        let st = sup.status().await;
        assert_eq!(st.len(), 1);
        assert!(!st[0].running, "not running before start");

        sup.start(ManagedProc::Snapserver).await.unwrap();
        let st = sup.status().await;
        assert!(st[0].running, "running after start");
        assert!(st[0].pid.is_some());

        // A second start while running is rejected.
        assert!(sup.start(ManagedProc::Snapserver).await.is_err());

        sup.stop(ManagedProc::Snapserver).await.unwrap();
        let st = sup.status().await;
        assert!(!st[0].running, "stopped after stop");
        let _ = std::fs::remove_file(&log);
    }

    #[tokio::test]
    async fn start_unknown_program_errors() {
        let sup = one_spec(
            ManagedProc::MpvWeb,
            ProcSpec {
                program: PathBuf::from("/nonexistent/xyzzy-not-real-binary"),
                args: vec![],
                log_path: std::env::temp_dir().join("ambient-sup-none.log"),
            },
        );
        assert!(sup.start(ManagedProc::MpvWeb).await.is_err());
    }
}
