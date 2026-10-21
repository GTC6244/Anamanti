#!/usr/bin/env bash
# Fetch the ggml Whisper models for the in-process STT engine
# (plans/python-to-rust-whisper.md). By default downloads BOTH `base` and `small`
# (English-only `.en` models), which the `stt.model` config selects between at runtime.
#
# Usage:
#   anamanti-core/scripts/fetch-whisper-models.sh [DEST_DIR]
#   anamanti-core/scripts/fetch-whisper-models.sh --multilingual [DEST_DIR]
#
# DEST_DIR defaults to ./models — the `stt.model_dir` default. The engine resolves
# `stt.model = "base" | "small"` to `<DEST_DIR>/ggml-<model>.en.bin`.
#
# --multilingual fetches the MULTILINGUAL models instead (`small` and
# `large-v3-turbo`, no `.en` suffix) — required for bilingual mode
# (plans/BilingualPlan.md §2.3): an `.en` model can only transcribe English. Select one
# with `"stt": { "model": "small", "multilingual": true }`.
set -euo pipefail

MULTI=0
if [ "${1:-}" = "--multilingual" ]; then
  MULTI=1
  shift
fi
DEST="${1:-models}"
BASE_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main"

if [ "$MULTI" = 1 ]; then
  MODELS="small large-v3-turbo"
else
  MODELS="base.en small.en"
fi

mkdir -p "$DEST"
for m in $MODELS; do
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
if [ "$MULTI" = 1 ]; then
  echo '  "stt": { "engine": "whisper-rs", "model": "small", "multilingual": true, "model_dir": "'"$DEST"'" }'
else
  echo '  "stt": { "engine": "whisper-rs", "model": "base", "model_dir": "'"$DEST"'" }'
fi
echo "and build/run with --features stt-whisper-local (or stt-whisper-metal)."
