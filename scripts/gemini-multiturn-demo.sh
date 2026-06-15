#!/usr/bin/env bash
# Live proof: gemini multi-turn through tmuxai (vim NORMAL-mode fix).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
export TMUX_AI_SOCKET=/tmp/tmux-ai-demo.sock
export TMUX_AI_PROFILES="$PWD/profiles"
export TMUX_AI_DAEMON="$PWD/target/release/tmux-ai-daemon"
TMUXAI="$PWD/target/release/tmuxai"

cleanup() { "$TMUXAI" kill gem-mt >/dev/null 2>&1 || true; }
trap cleanup EXIT

"$TMUXAI" spawn gem-mt --profile gemini-cli >/dev/null
for i in 1 2 3; do
  if ! "$TMUXAI" send gem-mt "Reply with exactly: TURN_${i}_OK" --timeout 120 >/dev/null; then
    echo "FAIL: send/wait failed (timeout?) on turn $i" >&2
    exit 1
  fi
  TEXT=$("$TMUXAI" text gem-mt)
  echo "turn $i -> $TEXT"
  case "$TEXT" in
    *TURN_${i}_OK*) ;;
    *) echo "FAIL: turn $i did not produce TURN_${i}_OK" >&2; exit 1 ;;
  esac
done
echo "PASS: 3 gemini turns through tmuxai"
