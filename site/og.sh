#!/usr/bin/env bash
# Renders the 1200x630 share card (src/og.html + brand.json) to dist/og.png with headless Chrome.
# Run after changing the name, tagline or command. Needs google-chrome (or $CHROME) and network
# access to Google Fonts.
set -euo pipefail
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
python3 - "$tmp/og.html" <<'PY'
import json, re, sys, pathlib
brand = json.load(open("brand.json"))
out = re.sub(r"\{\{\s*([\w.]+)\s*\}\}", lambda m: str(brand[m.group(1)]), pathlib.Path("src/og.html").read_text())
pathlib.Path(sys.argv[1]).write_text(out)
PY
mkdir -p dist
"${CHROME:-google-chrome}" --headless=new --disable-gpu --hide-scrollbars --no-sandbox \
  --virtual-time-budget=6000 --window-size=1200,630 --screenshot="$PWD/dist/og.png" "file://$tmp/og.html" 2>/dev/null
echo "wrote dist/og.png"
