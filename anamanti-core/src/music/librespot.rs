//! Keeps the librespot Connect device on the **Core's** Spotify account.
//!
//! librespot is one Connect device that any household member can cast to from their
//! phone (zeroconf). Casting signs librespot in to *that* person's account and its
//! cache (`credentials.json`) keeps them signed in across restarts — after which the
//! device is invisible to the Core's Web API token (a Connect device is only listed for
//! the account it is signed in to), so every voice request fails with "isn't
//! available". Seen live: one cast from another account on 2026-10-30 hid the device
//! from the Core for weeks.
//!
//! So the Core **pins** a copy of librespot's credentials whenever they are its own
//! account's, and when a command can't find the device it **reclaims** it: restores the
//! pinned credentials into the cache and restarts librespot (snapserver respawns its
//! `librespot://` child within a second or two). Others can still cast; a voice request
//! simply takes the speaker back. Restarting also recovers a librespot whose Spotify
//! session silently died.
//!
//! Like the rest of `crate::music` this never touches audio — only a credentials file
//! and the librespot process.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

/// librespot's credentials file inside its `--cache` directory.
const CREDENTIALS_FILE: &str = "credentials.json";

/// The librespot credentials cache plus the Core's pinned copy of its own account's
/// credentials.
#[derive(Debug, Clone)]
pub struct LibrespotAccount {
    /// librespot's `--cache` directory (must match `cache=` in `snapserver.conf`).
    cache_dir: PathBuf,
    /// Where the Core keeps its pinned copy of its own account's credentials.
    pin_path: PathBuf,
}

impl LibrespotAccount {
    pub fn new(cache_dir: impl Into<PathBuf>, pin_path: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            pin_path: pin_path.into(),
        }
    }

    fn credentials_path(&self) -> PathBuf {
        self.cache_dir.join(CREDENTIALS_FILE)
    }

    /// The Spotify username librespot is currently signed in as, if its cache has one.
    pub fn cached_username(&self) -> Option<String> {
        username_in(&self.credentials_path())
    }

    /// Whether a pinned copy of the Core's credentials exists.
    pub fn has_pin(&self) -> bool {
        username_in(&self.pin_path).is_some()
    }

    /// Pin librespot's current credentials when they belong to `account` (the Core's
    /// Spotify user id). Returns `true` when the pin was written or refreshed. A no-op
    /// when librespot is signed in to someone else or the pin is already current.
    pub fn remember(&self, account: &str) -> Result<bool> {
        let src = self.credentials_path();
        if username_in(&src).as_deref() != Some(account) {
            return Ok(false);
        }
        let creds = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
        if std::fs::read(&self.pin_path).ok().as_deref() == Some(creds.as_slice()) {
            return Ok(false);
        }
        write_private(&self.pin_path, &creds)?;
        Ok(true)
    }

    /// Copy the pinned credentials back into librespot's cache so its next start signs
    /// in to the Core's account. `Ok(false)` when nothing is pinned yet.
    pub fn restore_pinned(&self) -> Result<bool> {
        let Ok(creds) = std::fs::read(&self.pin_path) else {
            return Ok(false);
        };
        std::fs::create_dir_all(&self.cache_dir).ok();
        write_private(&self.credentials_path(), &creds)?;
        Ok(true)
    }

    /// Restore the pinned credentials (if any) and restart the librespot running as
    /// Connect device `device_name`; snapserver respawns it. Returns whether pinned
    /// credentials were restored.
    pub async fn reclaim(&self, device_name: &str) -> Result<bool> {
        let restored = self.restore_pinned()?;
        let pattern = format!("librespot --name {device_name}");
        let status = tokio::process::Command::new("pkill")
            .arg("-f")
            .arg(&pattern)
            .status()
            .await
            .context("running pkill to restart librespot")?;
        // Exit 1 = nothing matched (librespot is down; snapserver respawns it anyway).
        if !status.success() && status.code() != Some(1) {
            anyhow::bail!("pkill {pattern:?} failed: {status}");
        }
        Ok(restored)
    }
}

/// The `username` in a librespot credentials file, if it parses.
fn username_in(path: &Path) -> Option<String> {
    let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    v.get("username")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Write `bytes` to `path` with owner-only permissions (it is a login credential).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(user: &str) -> String {
        format!(r#"{{"username":"{user}","auth_type":1,"auth_data":"blob-{user}"}}"#)
    }

    fn account(dir: &Path) -> LibrespotAccount {
        LibrespotAccount::new(dir.join("cache"), dir.join("pin.json"))
    }

    /// A fresh, empty per-test directory under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ambient-librespot-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn remembers_only_the_cores_own_account() {
        let dir = scratch("remember");
        let acct = account(&dir);
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        let cache = dir.join("cache/credentials.json");

        std::fs::write(&cache, creds("someone-else")).unwrap();
        assert!(!acct.remember("me").unwrap());
        assert!(!acct.has_pin());

        std::fs::write(&cache, creds("me")).unwrap();
        assert!(acct.remember("me").unwrap());
        assert!(acct.has_pin());
        // Already current → no rewrite.
        assert!(!acct.remember("me").unwrap());
    }

    #[test]
    fn restore_puts_the_pinned_account_back() {
        let dir = scratch("restore");
        let acct = account(&dir);
        // Nothing pinned yet.
        assert!(!acct.restore_pinned().unwrap());

        std::fs::create_dir_all(dir.join("cache")).unwrap();
        let cache = dir.join("cache/credentials.json");
        std::fs::write(&cache, creds("me")).unwrap();
        acct.remember("me").unwrap();

        // Someone else casts and librespot caches their login …
        std::fs::write(&cache, creds("someone-else")).unwrap();
        assert_eq!(acct.cached_username().as_deref(), Some("someone-else"));
        // … and reclaiming restores the Core's.
        assert!(acct.restore_pinned().unwrap());
        assert_eq!(acct.cached_username().as_deref(), Some("me"));
    }
}
