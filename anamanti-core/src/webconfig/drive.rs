//! The Photos tab: Google Drive OAuth client credentials, folder picker and the
//! one-time consent. Secrets never leave through these responses — they reach the
//! tablet only over the device Wyoming hop.

use serde_json::{json, Value};

use super::{fail, keep_or_set};
use crate::settings::{DriveUpdate, SharedSettings};

/// `GET /drive/status.json` — the Drive photo linkage state for the `/drive` page.
/// Reports only booleans + folder ids + scope; the client secret and refresh token
/// are never included (they leave the orchestrator only over the device Wyoming hop).
pub(super) fn drive_status_json(settings: &SharedSettings) -> String {
    let d = settings.drive();
    json!({
        "ok": true,
        "configured": d.configured(),
        "linked": d.linked(),
        "client_id_set": d.client_id.as_deref().is_some_and(|s| !s.is_empty()),
        "client_secret_set": d.client_secret.as_deref().is_some_and(|s| !s.is_empty()),
        "has_refresh_token": d.refresh_token.as_deref().is_some_and(|s| !s.is_empty()),
        "folder_ids": d.folder_ids,
        "scope": d.scope.unwrap_or_default(),
    })
    .to_string()
}

/// `POST /drive/save` — set the Drive OAuth client id/secret and folder ids. A blank
/// or absent credential is left unchanged (a page reload never wipes a stored
/// secret); `folder_ids` (array or comma string) always replaces the stored list.
pub(super) fn drive_save_json(settings: &SharedSettings, data: &Value) -> String {
    let folder_ids = data.get("folder_ids").and_then(|v| {
        if let Some(arr) = v.as_array() {
            Some(
                arr.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>(),
            )
        } else {
            v.as_str().map(|s| {
                s.split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect()
            })
        }
    });
    settings.apply_drive(&DriveUpdate {
        client_id: keep_or_set(data, "client_id"),
        client_secret: keep_or_set(data, "client_secret"),
        folder_ids,
        ..Default::default()
    });
    drive_status_json(settings)
}

/// `GET /drive/folders.json` — list the linked account's Drive folders so the Photos
/// page can offer a picker instead of hand-typed folder ids. Mints an access token
/// from the stored refresh token; never returns secrets.
pub(super) async fn drive_folders_json(settings: &SharedSettings) -> String {
    let d = settings.drive();
    let (Some(cid), Some(secret), Some(rt)) = (
        d.client_id.clone().filter(|s| !s.is_empty()),
        d.client_secret.clone().filter(|s| !s.is_empty()),
        d.refresh_token.clone().filter(|s| !s.is_empty()),
    ) else {
        return fail("Link Google Drive first (set the client id/secret, then Link).");
    };
    let token = match crate::drive_consent::mint_access_token(&cid, &secret, &rt).await {
        Ok(t) => t,
        Err(e) => return fail(format!("{e:#}")),
    };
    match crate::drive_consent::list_folders(&token, 500).await {
        Ok(folders) => json!({ "ok": true, "folders": folders }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `POST /drive/link` — run the one-time OAuth consent using the stored client
/// credentials (opens a browser on the Mac), store the resulting refresh token, and
/// verify the configured folders with the immediate access token. Blocks this
/// connection until consent completes or times out — fine for a single admin request
/// on the loopback page.
pub(super) async fn drive_link_json(settings: &SharedSettings) -> String {
    let d = settings.drive();
    let (Some(cid), Some(secret)) = (
        d.client_id.clone().filter(|s| !s.is_empty()),
        d.client_secret.clone().filter(|s| !s.is_empty()),
    ) else {
        return fail("Set the Drive client id and secret first (a 'Desktop app' OAuth client).");
    };
    let scope = d
        .scope
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::config::DEFAULT_DRIVE_SCOPE.to_string());
    let outcome = match crate::drive_consent::run_consent(
        &cid,
        &secret,
        &scope,
        std::time::Duration::from_secs(180),
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return fail(format!("{e:#}")),
    };
    settings.apply_drive(&DriveUpdate {
        refresh_token: Some(Some(outcome.refresh_token.clone())),
        scope: Some(Some(outcome.scope.clone())),
        ..Default::default()
    });
    // Best-effort verification of each folder with the freshly minted access token.
    let mut verify = Vec::new();
    if let Some(at) = &outcome.access_token {
        for fid in &d.folder_ids {
            match crate::drive_consent::verify_folder(at, fid).await {
                Ok(n) => verify.push(json!({ "folder_id": fid, "images": n })),
                Err(e) => verify.push(json!({ "folder_id": fid, "error": format!("{e:#}") })),
            }
        }
    }
    let now = settings.drive();
    json!({
        "ok": true,
        "message": "Linked. The tablet picks this up on its next refresh (or reboot).",
        "linked": now.linked(),
        "folder_ids": now.folder_ids,
        "verify": verify,
    })
    .to_string()
}
