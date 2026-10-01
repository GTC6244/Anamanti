#!/usr/bin/env bash
# Fetch the ggml Whisper models for the in-process STT engine
# (plans/python-to-rust-whisper.md). Downloads BOTH `base` and `small` (English),
# which the `stt.model` config selects between at runtime.
#
# Usage:
#   anamanti-core/scripts/fetch-whisper-models.sh [DEST_DIR]
#
# DEST_DIR defaults to ./models — the `stt.model_dir` default. The engine resolves
# `stt.model = "base" | "small"` to `<DEST_DIR>/ggml-<model>.en.bin`.
set -euo pipefail

DEST="${1:-models}"
BASE_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"

mkdir -p "$DEST"
for m in base.en small.en; do
  out="$DEST/ggml-${m}.bin"
  if [ -f "$out" ]; then
    echo "✓ $out already present"
    continue
  fi
  echo "↓ ggml-${m}.bin → $out"
  curl -fL --progress-bar -o "$out" "${BASE_URL}/ggml-${m}.bin"
done

echo
echo "Done. Models in: $DEST"
echo "Enable the in-process engine in anamanti.json:"
echo '  "stt": { "engine": "whisper-rs", "model": "base", "model_dir": "'"$DEST"'" }'
echo "and build/run with --features stt-whisper-local."
