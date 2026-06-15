#!/usr/bin/env bash
# Live proof: parallel fan-out — 2 kiro (opus 4.6) + 2 gemini (3.5 flash).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
export TMUX_AI_SOCKET=/tmp/tmux-ai-demo.sock
export TMUX_AI_PROFILES="$PWD/profiles"
export TMUX_AI_DAEMON="$PWD/target/release/tmux-ai-daemon"
TMUXAI="$PWD/target/release/tmuxai"

TMP=$(mktemp -d)
cat > "$TMP/tasks.json" <<'EOF'
[{"id":"t1","prompt":"Reply with exactly: FAN_T1"},
 {"id":"t2","prompt":"Reply with exactly: FAN_T2"},
 {"id":"t3","prompt":"Reply with exactly: FAN_T3"},
 {"id":"t4","prompt":"Reply with exactly: FAN_T4"}]
EOF

"$TMUXAI" fanout --workers kiro:2,gemini:2 --tasks "$TMP/tasks.json" --out "$TMP/results" --timeout 240

python3 - "$TMP/results" <<'EOF'
import json, sys, pathlib
out = pathlib.Path(sys.argv[1])
summary = json.loads((out / "summary.json").read_text())
assert summary["ok"] == 4, f"expected 4 ok, got {summary}"
for i in range(1, 5):
    r = json.loads((out / f"t{i}.json").read_text())
    assert r["text"] and f"FAN_T{i}" in r["text"], f"t{i}: bad text {r['text']!r}"
assert summary["wall_secs"] < summary["sum_task_secs"], (
    f"no parallelism: wall {summary['wall_secs']:.1f}s >= sum {summary['sum_task_secs']:.1f}s")
print(f"PASS: 4/4 tasks ok; wall {summary['wall_secs']:.1f}s < serial {summary['sum_task_secs']:.1f}s")
EOF
