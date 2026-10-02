#!/usr/bin/env bash
# Xcode post-build phase of the Unlatch target: write the SMAppService LaunchAgent plist into
# Unlatch.app/Contents/Library/LaunchAgents (review §2(a)2). The engine runs as this agent:
# launchd starts `Unlatch --agent` on demand when a client looks up the MachService, and KeepAlive
# brings it back if it exits. Names come from Signing.xcconfig, never hard-coded.
#
# Needs Xcode's environment: TARGET_BUILD_DIR, CONTENTS_FOLDER_PATH, EXECUTABLE_NAME,
# PRODUCT_BUNDLE_IDENTIFIER, UNLATCH_BUNDLE_PREFIX, UNLATCH_APP_GROUP.
set -euo pipefail

: "${TARGET_BUILD_DIR:?run from Xcode}" "${CONTENTS_FOLDER_PATH:?}" "${EXECUTABLE_NAME:?}"
: "${PRODUCT_BUNDLE_IDENTIFIER:?}" "${UNLATCH_BUNDLE_PREFIX:?}" "${UNLATCH_APP_GROUP:?}"

# These land in XML unescaped: allow only what bundle ids and group ids may contain.
for v in "$PRODUCT_BUNDLE_IDENTIFIER" "$UNLATCH_BUNDLE_PREFIX" "$UNLATCH_APP_GROUP" "$EXECUTABLE_NAME"; do
  if [[ ! "$v" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "error: gen-launchagent: invalid identifier '$v'" >&2
    exit 1
  fi
done

label="$UNLATCH_BUNDLE_PREFIX.unlatch.agent"
dir="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Library/LaunchAgents"
out="$dir/$label.plist"
mkdir -p "$dir"
cat > "$out" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$label</string>
	<key>BundleProgram</key>
	<string>Contents/MacOS/$EXECUTABLE_NAME</string>
	<key>ProgramArguments</key>
	<array>
		<string>Contents/MacOS/$EXECUTABLE_NAME</string>
		<string>--agent</string>
	</array>
	<key>MachServices</key>
	<dict>
		<key>$UNLATCH_APP_GROUP.engine</key>
		<true/>
	</dict>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>$PRODUCT_BUNDLE_IDENTIFIER</string>
	</array>
</dict>
</plist>
PLIST
if command -v plutil >/dev/null; then
  plutil -lint "$out" >/dev/null
fi
echo "gen-launchagent: wrote $out" >&2
