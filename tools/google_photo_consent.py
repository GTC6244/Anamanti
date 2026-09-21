#!/usr/bin/env python3
"""One-time Google Drive consent helper for the ambient photo slideshow (Mac side).

Why this exists: the interim photo source is Google Drive (`drive.readonly`), but
the Echo Show can't consent on-device — Google's device-code/QR flow rejects Drive
scopes. So we do the OAuth *consent* once here on the Mac (real browser + keyboard),
get a **refresh token**, and adb-push it to the device. The device then refreshes +
reads Drive directly (data + token stay on-device; only consent happens here).

Flow: OAuth 2.0 authorization-code + PKCE with a **loopback** redirect
(127.0.0.1:<ephemeral port>). Requires a Google Cloud OAuth client of type
**"Desktop app"** (the TV/Ambient client cannot do loopback).

Credentials: read from display/google_oauth.json GOOGLE_DRIVE_CLIENT_ID/SECRET by
default (the same file the device build uses), or pass --client-id/--client-secret.

Usage:
    python3 tools/google_photo_consent.py \
        --folder-ids 1AbC...,1XyZ... --verify --push --serial G0918309009403GL
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import http.server
import json
import os
import secrets
import subprocess
import sys
import urllib.parse
import urllib.request
import webbrowser

AUTH_ENDPOINT = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_ENDPOINT = "https://oauth2.googleapis.com/token"
DEFAULT_SCOPE = "https://www.googleapis.com/auth/drive.readonly"
ANDROID_PACKAGE = "com.ambientdisplay.ambient_display"
TOKEN_FILENAME = "google_drive_token.json"

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_CREDS = os.path.join(REPO_ROOT, "display", "google_oauth.json")


def load_creds(args: argparse.Namespace) -> tuple[str, str]:
    if args.client_id and args.client_secret:
        return args.client_id, args.client_secret
    if not os.path.exists(args.creds):
        sys.exit(
            f"No credentials: pass --client-id/--client-secret or create {args.creds}.\n"
            "These must be a Google Cloud OAuth client of type 'Desktop app'."
        )
    with open(args.creds) as fh:
        data = json.load(fh)
    cid = data.get("GOOGLE_DRIVE_CLIENT_ID", "")
    secret = data.get("GOOGLE_DRIVE_CLIENT_SECRET", "")
    if not cid or not secret:
        sys.exit(
            f"{args.creds} is missing GOOGLE_DRIVE_CLIENT_ID / _SECRET "
            "(the 'Desktop app' client for Drive)."
        )
    return cid, secret


def pkce_pair() -> tuple[str, str]:
    verifier = base64.urlsafe_b64encode(secrets.token_bytes(40)).rstrip(b"=").decode()
    challenge = (
        base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest())
        .rstrip(b"=")
        .decode()
    )
    return verifier, challenge


class _CodeCatcher(http.server.BaseHTTPRequestHandler):
    code: str | None = None
    error: str | None = None

    def do_GET(self):  # noqa: N802
        params = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        _CodeCatcher.code = params.get("code", [None])[0]
        _CodeCatcher.error = params.get("error", [None])[0]
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        msg = (
            "Linked. You can close this tab and return to the terminal."
            if _CodeCatcher.code
            else f"Consent failed: {_CodeCatcher.error}. Close this tab."
        )
        self.wfile.write(f"<html><body><h2>{msg}</h2></body></html>".encode())

    def log_message(self, *_):
        pass


def run_consent(client_id: str, client_secret: str, scope: str) -> dict:
    verifier, challenge = pkce_pair()
    server = http.server.HTTPServer(("127.0.0.1", 0), _CodeCatcher)
    redirect_uri = f"http://127.0.0.1:{server.server_address[1]}"
    auth_url = AUTH_ENDPOINT + "?" + urllib.parse.urlencode(
        {
            "client_id": client_id,
            "redirect_uri": redirect_uri,
            "response_type": "code",
            "scope": scope,
            "access_type": "offline",
            "prompt": "consent",
            "code_challenge": challenge,
            "code_challenge_method": "S256",
        }
    )
    print("\nOpening your browser to approve access…")
    print("If it doesn't open, paste this URL:\n  " + auth_url + "\n")
    webbrowser.open(auth_url)
    server.handle_request()
    if _CodeCatcher.error or not _CodeCatcher.code:
        sys.exit(f"Consent failed: {_CodeCatcher.error or 'no code returned'}")

    body = urllib.parse.urlencode(
        {
            "client_id": client_id,
            "client_secret": client_secret,
            "code": _CodeCatcher.code,
            "code_verifier": verifier,
            "grant_type": "authorization_code",
            "redirect_uri": redirect_uri,
        }
    ).encode()
    req = urllib.request.Request(
        TOKEN_ENDPOINT,
        data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return json.load(resp)
    except urllib.error.HTTPError as e:
        sys.exit(f"Token exchange failed ({e.code}): {e.read().decode()}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--scope", default=DEFAULT_SCOPE)
    ap.add_argument("--creds", default=DEFAULT_CREDS)
    ap.add_argument("--client-id")
    ap.add_argument("--client-secret")
    ap.add_argument("--folder-ids", default="", help="Comma-separated Drive folder IDs")
    ap.add_argument("--output", default=os.path.join(REPO_ROOT, "display", TOKEN_FILENAME))
    ap.add_argument("--push", action="store_true", help="adb-push the token to the device")
    ap.add_argument("--serial", help="adb device serial (with --push)")
    ap.add_argument("--verify", action="store_true", help="After consent, test-list the folders")
    args = ap.parse_args()

    client_id, client_secret = load_creds(args)
    tokens = run_consent(client_id, client_secret, args.scope)
    refresh_token = tokens.get("refresh_token")
    if not refresh_token:
        sys.exit(
            "No refresh_token returned. Re-run (uses prompt=consent) and confirm the "
            "client is type 'Desktop app'."
        )

    folder_ids = [f.strip() for f in args.folder_ids.split(",") if f.strip()]
    payload = {"refresh_token": refresh_token, "folder_ids": folder_ids, "scope": args.scope}
    with open(args.output, "w") as fh:
        json.dump(payload, fh, indent=2)
    os.chmod(args.output, 0o600)
    print(f"\n✅ Wrote {args.output} (refresh token + {len(folder_ids)} folder id(s)).")

    if args.verify:
        _verify(tokens.get("access_token"), folder_ids)
    if args.push:
        _push(args.output, args.serial)
    else:
        dest = f"/sdcard/Android/data/{ANDROID_PACKAGE}/files/{TOKEN_FILENAME}"
        print(
            "\nTo sync to the device, run:\n"
            f"  adb push {args.output} {dest}\n"
            "then in the app: Settings > Idle photos > Google Drive > Import token."
        )


def _verify(access_token: str | None, folder_ids: list[str]) -> None:
    if not access_token or not folder_ids:
        print("(verify skipped: need an access token and --folder-ids)")
        return
    for fid in folder_ids:
        q = f"'{fid}' in parents and mimeType contains 'image/' and trashed = false"
        url = "https://www.googleapis.com/drive/v3/files?" + urllib.parse.urlencode(
            {"q": q, "fields": "files(id,name)", "pageSize": "5", "corpora": "user"}
        )
        req = urllib.request.Request(url, headers={"Authorization": f"Bearer {access_token}"})
        try:
            with urllib.request.urlopen(req) as resp:
                files = json.load(resp).get("files", [])
            print(f"  folder {fid}: {len(files)} image(s) visible "
                  f"{'✅' if files else '⚠️  (empty — check folder id / sharing)'}")
        except urllib.error.HTTPError as e:
            print(f"  folder {fid}: ❌ {e.code} {e.read().decode()[:120]}")


def _push(path: str, serial: str | None) -> None:
    dest = f"/sdcard/Android/data/{ANDROID_PACKAGE}/files/{TOKEN_FILENAME}"
    cmd = ["adb"] + (["-s", serial] if serial else []) + ["push", path, dest]
    print("\n$ " + " ".join(cmd))
    try:
        subprocess.run(cmd, check=True)
        print("✅ Pushed. In the app: Settings > Idle photos > Google Drive > Import token.")
    except (subprocess.CalledProcessError, FileNotFoundError) as e:
        print(f"⚠️  adb push failed ({e}). Push it manually:\n  adb push {path} {dest}")


if __name__ == "__main__":
    main()
