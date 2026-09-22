#!/usr/bin/env bash
# Ambient music routing — Mac host bring-up (P1–P3 of plans/snapcast_routing_plan.md).
#
# Installs snapserver + librespot + mpv (via Homebrew), creates the two snapfifos,
# and installs the snapserver.conf. It does NOT load the launchd agents or start
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

echo "==> Creating snapfifos in $RUN_DIR"
mkdir -p "$RUN_DIR"
for fifo in snap-spotify snap-web; do
  if [ -p "$RUN_DIR/$fifo" ]; then
    echo "    $RUN_DIR/$fifo exists"
  else
    mkfifo "$RUN_DIR/$fifo"
    echo "    created $RUN_DIR/$fifo"
  fi
done

echo "==> Installing snapserver.conf → $CONF_DST"
if [ -f "$CONF_DST" ] && ! cmp -s "$CONF_SRC" "$CONF_DST"; then
  cp "$CONF_DST" "$CONF_DST.bak.$(date +%s)"
  echo "    backed up existing config"
fi
cp "$CONF_SRC" "$CONF_DST"

cat <<EOF

Done. Next (manual — see deploy/snapcast/README.md):
  1. Start snapserver:   snapserver -c "$CONF_DST"
  2. Start librespot:    librespot --name Ambient --backend pipe --device "$RUN_DIR/snap-spotify" --bitrate 320
  3. (web) Start mpv:    mpv --idle=yes --no-video --input-ipc-server=/tmp/ambient-mpv.sock \\
                              --ao=pcm --ao-pcm-waveheader=no --ao-pcm-file="$RUN_DIR/snap-web" \\
                              --audio-samplerate=48000 --audio-channels=stereo --audio-format=s16
  4. Point a snapclient at this Mac (Linux unit in deploy/snapcast/linux/).
  5. Enable ducking on the orchestrator: AMBIENT_MUSIC=on (see README).
Or install the launchd agents in deploy/snapcast/launchd/ once validated.
EOF
