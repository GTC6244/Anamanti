#!/usr/bin/env bash
# Fetch the Silero VAD v4 ONNX model for the neural VAD engine
# (plans/VadSileroPlan.md). The `vad-silero` build feature runs this model on
# onnxruntime; pure-Rust tract cannot load it (the graph's `If` op).
#
# Usage:
#   anamanti-core/scripts/fetch-vad-model.sh [DEST_DIR]
#
# DEST_DIR defaults to ./models — the `vad.silero.model_path` default dir. Enable in
# anamanti.json:
#   "vad": { "engine": "silero", "silero": { "model_path": "models/silero_vad.onnx" } }
# and build with `--features vad-silero`.
set -euo pipefail

DEST="${1:-models}"
OUT="$DEST/silero_vad.onnx"
# Silero VAD v4 (MIT-licensed; v5 export misbehaves under onnxruntime), pinned to the snakers4/silero-vad repo.
URL="https://github.com/snakers4/silero-vad/raw/v4.0/files/silero_vad.onnx"
# Expected SHA-256 of the v4 model (integrity check; update if upstream re-releases).
EXPECTED_SHA="a35ebf52fd3ce5f1469b2a36158dba761bc47b973ea3382b3186ca15b1f5af28"

mkdir -p "$DEST"
if [ -f "$OUT" ]; then
  echo "✓ $OUT already present"
else
  echo "↓ silero_vad.onnx → $OUT"
  curl -fL --progress-bar -o "$OUT" "$URL"
fi

if command -v shasum >/dev/null 2>&1; then
  GOT="$(shasum -a 256 "$OUT" | awk '{print $1}')"
  if [ "$GOT" != "$EXPECTED_SHA" ]; then
    echo "⚠ WARNING: $OUT sha256 $GOT != expected $EXPECTED_SHA" >&2
    echo "  (upstream may have re-released the model; verify before shipping)" >&2
  else
    echo "✓ sha256 verified"
  fi
fi

echo
echo "Done. Model in: $OUT"
echo "Enable in anamanti.json and build with --features vad-silero:"
echo '  "vad": { "engine": "silero", "silero": { "model_path": "'"$OUT"'" } }'
