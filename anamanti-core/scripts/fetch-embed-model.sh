#!/usr/bin/env bash
# Fetch nomic-embed-text-v1.5 (ONNX + tokenizer) for the local GraphRAG memory
# embedder (feature `embed-local`, ON by default). The embedder runs this model
# in-process via fastembed/onnxruntime — no network, no OPENAI_API_KEY at runtime.
#
# Usage:
#   anamanti-core/scripts/fetch-embed-model.sh [DEST_DIR]
#
# DEST_DIR defaults to ./models — the `graphrag.embed_model_path` /
# `embed_tokenizer_dir` default dir. The defaults in anamanti.json are:
#   "graphrag": {
#     "embed_backend": "local",
#     "embed_model_path": "models/nomic-embed-text-v1.5.onnx",
#     "embed_tokenizer_dir": "models/nomic-tokenizer"
#   }
# Build with the (default) `embed-local` feature.
set -euo pipefail

DEST="${1:-models}"
TOKDIR="$DEST/nomic-tokenizer"
MODEL="$DEST/nomic-embed-text-v1.5.onnx"
BASE="https://huggingface.co/nomic-ai/nomic-embed-text-v1.5/resolve/main"

# Expected SHA-256s (integrity check; update if upstream re-releases). Pinned to the
# `main` revision provisioned 2026-10-01.
MODEL_SHA="147d5aa88c2101237358e17796cf3a227cead1ec304ec34b465bb08e9d952965"
declare -A TOK_SHA=(
  [tokenizer.json]="d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66"
  [config.json]="9ab00bd92cee80a569f708140b7b6c1661a65891ff3765b1519e181ba2f2c92b"
  [special_tokens_map.json]="5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a"
  [tokenizer_config.json]="d7e0000bcc80134debd2222220427e6bf5fa20a669f40a0d0d1409cc18e0a9bc"
)

mkdir -p "$TOKDIR"

verify() { # path expected_sha label
  if command -v shasum >/dev/null 2>&1; then
    local got
    got="$(shasum -a 256 "$1" | awk '{print $1}')"
    if [ "$got" != "$2" ]; then
      echo "⚠ WARNING: $1 sha256 $got != expected $2" >&2
      echo "  (upstream may have re-released; verify before shipping)" >&2
    else
      echo "✓ sha256 verified: $3"
    fi
  fi
}

# The ONNX model (~547 MB).
if [ -f "$MODEL" ]; then
  echo "✓ $MODEL already present"
else
  echo "↓ onnx/model.onnx → $MODEL"
  curl -fL --progress-bar -o "$MODEL" "$BASE/onnx/model.onnx"
fi
verify "$MODEL" "$MODEL_SHA" "model.onnx"

# The four tokenizer files fastembed's UserDefinedEmbeddingModel requires.
for f in tokenizer.json config.json special_tokens_map.json tokenizer_config.json; do
  out="$TOKDIR/$f"
  if [ -f "$out" ]; then
    echo "✓ $out already present"
  else
    echo "↓ $f → $out"
    curl -fL --progress-bar -o "$out" "$BASE/$f"
  fi
  verify "$out" "${TOK_SHA[$f]}" "$f"
done

echo
echo "Done. Model:     $MODEL"
echo "      Tokenizer: $TOKDIR/"
echo "Local embeddings are the default (graphrag.embed_backend=\"local\"); just run the Core."
