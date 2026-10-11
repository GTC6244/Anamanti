#!/usr/bin/env bash
# Regenerate the admin-console stylesheet from its Tailwind source.
#
# The Anamanti Core admin UI (the loopback config page) is plain static HTML styled
# with Tailwind CSS in a Material Design layout. Tailwind is vendored PRECOMPILED:
# this script runs the Tailwind CLI over the source (src/webconfig/tailwind/input.css,
# which @source-scans the page HTML + shell.js) and writes the committed, browser-served
# stylesheet src/webconfig/panel.css. That file is embedded into the binary via
# include_str! (see src/webconfig.rs) and served at GET /panel.css — so the single
# copied release binary still serves the whole UI with nothing beside it, fully offline
# (no CDN, no in-browser JIT).
#
# Re-run this after editing input.css (theme/components) OR any webconfig *.html / shell.js
# (so newly used utility classes get emitted). Commit the regenerated panel.css.
#
# Requires the `tailwindcss` CLI on PATH (standalone binary or npx). On this Mac it is
# installed at /opt/homebrew/bin/tailwindcss (Tailwind v4).
set -euo pipefail

# Resolve paths relative to this script so it works from any CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEBCONFIG_DIR="$SCRIPT_DIR/../src/webconfig"
INPUT="$WEBCONFIG_DIR/tailwind/input.css"
OUTPUT="$WEBCONFIG_DIR/panel.css"

if command -v tailwindcss >/dev/null 2>&1; then
  TW=(tailwindcss)
elif command -v npx >/dev/null 2>&1; then
  TW=(npx -y @tailwindcss/cli)
else
  echo "error: no 'tailwindcss' CLI and no 'npx' on PATH." >&2
  echo "       install Tailwind v4 (e.g. 'brew install tailwindcss') and re-run." >&2
  exit 1
fi

echo "Building $OUTPUT from $INPUT ..."
"${TW[@]}" --input "$INPUT" --output "$OUTPUT" --minify
echo "Done. Remember to commit src/webconfig/panel.css"
