#!/usr/bin/env bash
# Fetch the ECAPA-TDNN speaker-embedding ONNX model for per-person speaker ID
# (plans/speaker_id_plan.md + plans/SpeakerEnrollmentPlan.md). Pure-Rust
# `tract-onnx` loads + runs this model directly (verified 2026-10-11 via
# `cargo run --example speaker_smoke` — no onnxruntime needed), producing a 192-dim
# voiceprint from an 80-mel / 25 ms / 10 ms log-mel fbank front-end.
#
# Usage:
#   anamanti-core/scripts/fetch-speaker-model.sh [DEST_DIR]
#
# DEST_DIR defaults to ./models. Enable in anamanti.json:
#   "speaker": { "enabled": true, "model_path": "models/voxceleb_ECAPA512.onnx" }
set -euo pipefail

DEST="${1:-models}"
OUT="$DEST/voxceleb_ECAPA512.onnx"
# WeSpeaker VoxCeleb ECAPA-TDNN (c512, emb192, fbank80) ONNX export (Apache-2.0),
# pinned to the Wespeaker HF repo. embed_dim=192, feat_dim=80 (see its config.yaml).
URL="https://huggingface.co/Wespeaker/wespeaker-voxceleb-ecapa-tdnn512/resolve/main/voxceleb_ECAPA512.onnx"
# Expected SHA-256 (integrity check; update if upstream re-releases).
EXPECTED_SHA="f901ae3571912bad4e11e157744cb8b7d77a0b46923b83c410bdbb6c77d69736"

mkdir -p "$DEST"
if [ -f "$OUT" ]; then
  echo "✓ $OUT already present"
else
  echo "↓ voxceleb_ECAPA512.onnx → $OUT"
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
echo "Enable speaker ID in anamanti.json:"
echo '  "speaker": { "enabled": true, "model_path": "'"$OUT"'" }'
echo "Thresholds (match_threshold / new_threshold / min_speech_ms) default to"
echo "placeholders — calibrate against real Echo Show far-field captures."
