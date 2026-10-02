#!/usr/bin/env bash
# CI: sign an unsigned build ad hoc with its real (expanded) entitlements, inside out, then run
# `codesign --verify --strict` and check-bundle.py --signed against what codesign embedded.
# An ad-hoc build cannot run the Finder integration (docs/MACOS.md), but this catches broken
# nesting, unsigned helpers and entitlement drift on every PR, without secrets.
#
#   adhoc-sign.sh <Unlatch.app> <settings.json>
set -euo pipefail
app="${1:?usage: adhoc-sign.sh <Unlatch.app> <settings.json>}"
settings="${2:?usage: adhoc-sign.sh <Unlatch.app> <settings.json>}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ent="$(mktemp -d)"
trap 'rm -rf "$ent"' EXIT

python3 "$here/check-bundle.py" --settings "$settings" --expand-entitlements "$ent"

sign() { codesign --force --sign - --options runtime --timestamp=none "$@"; }
# Inside out: any loose dylibs first (none in a normal build), then the helper, appex and app.
while IFS= read -r -d '' lib; do
  sign "$lib"
done < <(find "$app" -name '*.dylib' -type f -print0)
sign "$app/Contents/MacOS/unlatch-askpass"
sign --entitlements "$ent/appex.entitlements" "$app/Contents/PlugIns/UnlatchFileProvider.appex"
sign --entitlements "$ent/app.entitlements" "$app"

codesign --verify --strict --deep --verbose=2 "$app"
python3 "$here/check-bundle.py" --settings "$settings" --app "$app" --signed "${@:3}"
