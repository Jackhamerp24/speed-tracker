#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
RUNTIME="${1:-win-x64}"
case "$RUNTIME" in win-x64|win-arm64) ;; *) echo "Usage: $0 [win-x64|win-arm64]" >&2; exit 2 ;; esac
DOTNET="${DOTNET:-dotnet}"
if ! command -v "$DOTNET" >/dev/null 2>&1 && [ -x /usr/local/share/dotnet/dotnet ]; then
    DOTNET=/usr/local/share/dotnet/dotnet
fi
OUT="$PWD/dist/SpeedTracker-$RUNTIME"
"$DOTNET" publish Windows/SpeedTracker.Windows/SpeedTracker.Windows.csproj -c Release -r "$RUNTIME" --self-contained true -o "$OUT"
"$DOTNET" publish Windows/SpeedTracker.Collector/SpeedTracker.Collector.csproj -c Release -r "$RUNTIME" --self-contained true -o "$OUT/collector"
(cd "$OUT" && zip -q -r "../SpeedTracker-$RUNTIME.zip" .)
printf 'Built %s.zip\n' "$OUT"
