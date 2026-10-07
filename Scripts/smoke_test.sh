#!/bin/bash
# End-to-end check of the optional proxy: runs the app against the mock provider on spare ports with
# a throwaway data folder, sends one call per API format through the proxy, and
# prints what was measured. Expect TTFT near 0.5 s and about 60 tok/s.
set -euo pipefail
cd "$(dirname "$0")/.."

PROXY_PORT=4199
MOCK_PORT=4198
HOME_DIR="$(mktemp -d)"
BIN="$(swift build -c release --show-bin-path)/SpeedTracker"
[ -x "$BIN" ] || swift build -c release

cat > "$HOME_DIR/config.json" <<JSON
{"port": $PROXY_PORT, "routes": {"mock": "http://127.0.0.1:$MOCK_PORT"}}
JSON

python3 Scripts/mock_upstream.py "$MOCK_PORT" > "$HOME_DIR/mock.log" 2>&1 &
MOCK_PID=$!
SPEEDTRACKER_HOME="$HOME_DIR" "$BIN" > "$HOME_DIR/app.log" 2>&1 &
APP_PID=$!
trap 'kill $MOCK_PID $APP_PID 2>/dev/null || true; rm -rf "$HOME_DIR"' EXIT

for _ in $(seq 1 50); do
    curl -fs "http://127.0.0.1:$PROXY_PORT/" > /dev/null 2>&1 && break
    sleep 0.1
done

BASE="http://127.0.0.1:$PROXY_PORT"
call() { # name, url, user agent, body
    echo "→ $1"
    curl -sN -o /dev/null -w "   http %{http_code}, %{time_total}s\n" -A "$3" \
        -H "Content-Type: application/json" -d "$4" "$2"
}
call "Anthropic Messages (as Claude Code)" "$BASE/mock/v1/messages?beta=true" "claude-cli/2.1.284 (external, cli)" \
    '{"model":"claude-opus-5-5","stream":true,"messages":[{"role":"user","content":"hi"}]}'
call "OpenAI Chat Completions with reasoning (DeepSeek style)" "$BASE/mock@deepseek/v1/chat/completions" "OpenAI/JS 6.2.0" \
    '{"model":"deepseek-reasoner","stream":true,"messages":[{"role":"user","content":"hi"}]}'
call "OpenAI Responses (as Codex)" "$BASE/mock/v1/responses" "codex_cli_rs/0.159.2 (Mac OS 27.0; arm64)" \
    '{"model":"gpt-6.1-sol","stream":true,"input":"hi"}'
call "Anthropic, not streamed (tagged omp)" "$BASE/mock@omp/v1/messages" "claude-cli/2.1.284" \
    '{"model":"claude-haiku-4-5","messages":[{"role":"user","content":"hi"}]}'
call "count_tokens (must not be recorded)" "$BASE/mock/v1/messages/count_tokens" "claude-cli/2.1.284" \
    '{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}'

sleep 1
echo
echo "Recorded through the proxy:"
python3 - "$HOME_DIR/history.jsonl" <<'PY'
import json, sys
# The app also picks up calls on its own from harness logs; this test is about the proxy.
rows = [row for row in (json.loads(line) for line in open(sys.argv[1])) if row.get("source") == "proxy"]
print(f"  {'harness':<13}{'model':<20}{'format':<18}{'ttft':>7}{'tok/s':>8}{'out':>6}  streamed")
for r in rows:
    ttft = f"{r['ttft']:.3f}" if r.get("ttft") is not None else "-"
    print(f"  {r['harness']:<13}{r['model']:<20}{r['format']:<18}{ttft:>7}{r['tps']:>8.1f}{r['outputTokens']:>6}  {r['streamed']}")
assert len(rows) == 4, f"expected 4 records, got {len(rows)}"
for r in rows:
    if r["streamed"]:
        assert 0.45 <= r["ttft"] <= 0.60, r
        assert 54 <= r["tps"] <= 66, r
print("  OK: streamed calls measured within tolerance of the mock's 0.5 s / 60 tok/s")
PY
