#!/bin/bash
# Runs the unit tests. With only the Command Line Tools installed, the Swift
# Testing macro plugin is not on the default search path, so point at it.
set -euo pipefail
cd "$(dirname "$0")/.."

PLUGINS="/Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing"
if [ -d "$PLUGINS" ] && ! xcodebuild -version >/dev/null 2>&1; then
    swift test -Xswiftc -plugin-path -Xswiftc "$PLUGINS" "$@"
else
    swift test "$@"
fi
