#!/bin/bash
# Builds "Speed Tracker.app" in the project folder. Needs only the Xcode Command Line Tools.
# UNIVERSAL=1 builds for Apple silicon and Intel together; that needs full Xcode, as CI has.
set -euo pipefail
cd "$(dirname "$0")/.."

ARCHS=""
if [ "${UNIVERSAL:-0}" = "1" ]; then ARCHS="--arch arm64 --arch x86_64"; fi
# shellcheck disable=SC2086
swift build -c release $ARCHS
# shellcheck disable=SC2086
BIN="$(swift build -c release $ARCHS --show-bin-path)/SpeedTracker"

APP="Speed Tracker.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/SpeedTracker"
cp Resources/Info.plist "$APP/Contents/Info.plist"

if [ ! -f Resources/AppIcon.icns ]; then
    swift Scripts/make_icon.swift Resources/AppIcon.icns
fi
cp Resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"

# Ad-hoc signature: enough to run locally and to register as a login item.
codesign --force --sign - "$APP"
echo "Built $APP"
