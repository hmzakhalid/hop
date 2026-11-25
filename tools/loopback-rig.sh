#!/bin/bash
# Pair and drive a dry-run agent on this Mac, over loopback, with throwaway keys.
# Touches no real pairing (HOP_DIR / HOP_AGENT_DIR point at a temp folder) and no real PC,
# and installs no event tap, so it is safe while a real `hop` is running.
# Usage: tools/loopback-rig.sh [port]   (default 47901)
set -euo pipefail
cd "$(dirname "$0")/.."
PORT=${1:-47901}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/hop-rig.XXXXXX")
export HOP_DIR="$WORK/mac" HOP_AGENT_DIR="$WORK/agent"
cargo build -q -p hop -p hop-agent
AGENT_LOG="$WORK/agent.log"
./target/debug/hop-agent --dry-run --port "$PORT" --name "Rig PC" >"$AGENT_LOG" 2>&1 &
AGENT=$!
trap 'kill $AGENT 2>/dev/null; wait $AGENT 2>/dev/null; rm -rf "$WORK"' EXIT
for _ in $(seq 50); do grep -q "Pairing code" "$AGENT_LOG" && break; sleep 0.1; done
CODE=$(sed -n 's/.*Pairing code: *\([0-9A-Z-]*\).*/\1/p' "$AGENT_LOG" | head -1)
[ -n "$CODE" ] || { cat "$AGENT_LOG"; echo "rig: no pairing code"; exit 1; }
echo "$CODE" | ./target/debug/hop pair --host "127.0.0.1:$PORT"
./target/debug/hop drive --host "127.0.0.1:$PORT"
sleep 0.3
echo "--- what the agent received ---"
grep -v "^$" "$AGENT_LOG" | tail -n 40
