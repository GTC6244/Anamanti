#!/usr/bin/env bash
# Ambient music routing — Mac host bring-up (P1–P3 of plans/snapcast_routing_plan.md).
#
# Installs snapserver + librespot + mpv (via Homebrew), creates the run dir (the
# librespot credentials cache), and installs the snapserver.conf. snapserver launches
# librespot + mpv itself (no named FIFOs — they stall on macOS; see snapserver.conf). It does NOT load the launchd agents or start
# any daemon — do that step manually (see the runbook, deploy/snapcast/README.md)
# so nothing grabs audio/ports unattended. Re-runnable (idempotent).
set -euo pipefail

PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
RUN_DIR="$PREFIX/var/run/ambient"
CONF_SRC="$(cd "$(dirname "$0")" && pwd)/snapserver.conf"
CONF_DST="$PREFIX/etc/snapserver.conf"

echo "==> Installing snapserver, librespot, mpv (Homebrew)"
for pkg in snapcast librespot mpv; do
  if brew list --formula 2>/dev/null | grep -qx "$pkg"; then
    echo "    $pkg already installed"
  else
    brew install "$pkg"
  fi
done

echo "==> Creating $RUN_DIR/librespot-cache"
mkdir -p "$RUN_DIR/librespot-cache"

echo "==> Installing snapserver.conf → $CONF_DST"
if [ -f "$CONF_DST" ] && ! cmp -s "$CONF_SRC" "$CONF_DST"; then
  # cp dereferences a symlink, so this captures the current content (the Homebrew
  # default snapserver.conf is a symlink into the Cellar).
  cp "$CONF_DST" "$CONF_DST.bak.$(date +%s)"
  echo "    backed up existing config"
fi
# Remove first so we replace a Homebrew symlink with a real file instead of writing
# through it into the Cellar (which `brew upgrade` would later clobber).
rm -f "$CONF_DST"
cp "$CONF_SRC" "$CONF_DST"

cat <<EOF

Done. Next (manual — see deploy/snapcast/README.md):
  1. Start snapserver (it launches librespot "Ambient" + the idle mpv itself):
       snapserver -c "$CONF_DST"
     or let the Anamanti Core start it ("music": {"enabled": true, "autostart": true}).
  2. Point a snapclient at this Mac (Linux unit in deploy/snapcast/linux/).
  3. Ducking/radio/volume: "music": {"enabled": true} in anamanti.json (see README).
EOF
