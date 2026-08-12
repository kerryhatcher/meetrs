#!/usr/bin/env bash
# Wrap the binary in a minimal .app bundle so macOS TCC will prompt for
# audio-capture consent. A bare binary has no Info.plist, so it can never
# carry the usage-description strings TCC requires, and consent silently
# never happens.
#
# ponytail: hand-rolled plist beats cargo-bundle for a POC. Switch to
# cargo-packager when this needs a real signed, notarized DMG.
set -euo pipefail

PROFILE="${1:-debug}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/$PROFILE/meetrs"
APP="$ROOT/target/$PROFILE/meetrs.app"

[ -x "$BIN" ] || { echo "no binary at $BIN — run: cargo build${PROFILE:+ --$PROFILE}" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/meetrs"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>meetrs</string>
  <key>CFBundleIdentifier</key><string>com.kerryhatcher.meetrs</string>
  <key>CFBundleName</key><string>meetrs</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>14.4</string>
  <!-- Core Audio process taps require this key; it is not in Xcode's dropdown. -->
  <key>NSAudioCaptureUsageDescription</key>
  <string>meetrs records system audio so meetings can be saved locally.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>meetrs records your microphone so meetings can be saved locally.</string>
</dict>
</plist>
PLIST

# TCC keys consent to the code-signing identity, so an unsigned build is never
# even prompted — and an ad-hoc one is re-prompted on every rebuild. sign.sh
# prefers the stable local identity and explains the fallback.
"$ROOT/scripts/sign.sh" "$APP" >/dev/null

echo "built $APP"
echo "run it from a terminal so the TUI gets a pty:"
echo "  $APP/Contents/MacOS/meetrs"
