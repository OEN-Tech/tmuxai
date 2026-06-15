# tmux-ai-parser

A Rust engine that parses terminal output from AI CLI tools running inside tmux, producing structured events for programmatic consumption.

Supports **Claude Code**, **Codex CLI**, **Kiro CLI**, and **Gemini CLI** via profile-driven recognizer patterns, with a self-updating LLM fallback for unknown output formats.

## Quick Start

### As a library

```rust
use tmux_ai_io::TmuxSession;
use tmux_ai_io::human_input::TypingProfile;
use tmux_ai_parser::{Parser, events::Event};
use tmux_ai_parser::profile::CompiledProfile;

#[tokio::main]
async fn main() -> Result<(), String> {
    // Load profile
    let profile = CompiledProfile::load("profiles/claude-code.toml".as_ref())?;
    let mut parser = Parser::new(profile);

    // Spawn a session
    let mut session = TmuxSession::spawn("my-session", "claude --dangerously-skip-permissions", ".").await?;

    // Send with human-like typing
    session.send_human("explain this codebase", &TypingProfile::default()).await?;

    // Wait for response and parse
    let events = session.wait_and_capture().await
        .map(|text| parser.parse_snapshot(&text))?;

    for event in events {
        match event {
            Event::AssistantText { text, .. } => println!("Response: {text}"),
            Event::Question { choices, .. } => {
                for c in choices { println!("  {}) {}", c.key, c.text); }
            }
            Event::Ready => println!("(waiting for input)"),
            _ => {}
        }
    }

    session.kill().await
}
```

### As a daemon

```bash
# Start the daemon
TMUX_AI_PROFILES=profiles cargo run --bin tmux-ai-daemon

# In another terminal, connect via Unix socket
python3 -c "
import socket, json
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect('/tmp/tmux-ai-parser.sock')

# Create a session
s.sendall(b'{\"create_session\":{\"name\":\"demo\",\"command\":\"claude --dangerously-skip-permissions\",\"cwd\":\".\"}}\\n')
print(s.recv(4096).decode())

# Send input
s.sendall(b'{\"send_input\":{\"session\":\"demo\",\"text\":\"say hello\"}}\\n')
print(s.recv(4096).decode())
"
```

## Supported CLIs

### Claude Code

```
Profile: profiles/claude-code.toml
Command: claude --dangerously-skip-permissions
Submit:  Enter
```

| Element | Pattern |
|---------|---------|
| Banner | `Claude Code vX.Y.Z` |
| Prompt | `❯` |
| Assistant text | `⏺ text` |
| Tool use | `⏺ Bash(command)` / `⏺ Read(file)` |
| Tool result | `  ⎿  output` |
| Thinking | `✻ Churning... (Ns)` |
| Skill load | `⏺ Skill(name)` |
| Checklist | `✔ done` / `◼ active` / `◻ pending` |
| Status bar | `Opus 4.6 (1M context)  ↓345 ↑18  $0.13  [██░░░] 3%  Off-Peak  MEM:97%` |

### Codex CLI

```
Profile: profiles/codex-cli.toml
Command: codex --full-auto
Submit:  Enter
```

| Element | Pattern |
|---------|---------|
| Banner | `OpenAI Codex (vX.Y.Z)` in `╭╰` box |
| Prompt | `›` |
| Assistant text | `• text` |
| Tool use | `• Ran command` / `• Explored` / `• Read file` |
| Tool result | `└ output` (tree branch) |
| Thinking | `◦ Working (Ns • esc to interrupt)` |
| Status bar | `gpt-5.4 xhigh · 97% left · ~/Code · weekly 31%` |

### Kiro CLI

```
Profile: profiles/kiro-cli.toml
Command: kiro-cli chat
Submit:  Enter
```

| Element | Pattern |
|---------|---------|
| Banner | ASCII art KIRO logo + `kiro-cli N.N.N` |
| Prompt | `N% λ >` (context % + lambda) |
| Assistant text | `> text` |
| Tool use | `I will run the following command: CMD (using tool: TOOL)` |
| Permission | `[y/n/t]:` |
| Tool result | `- Completed in Ns` |
| Timing | `▸ Time: Ns` |
| Skill load | `✓ name loaded in Ns` |

### Gemini CLI

```
Profile: profiles/gemini-cli.toml
Command: gemini
Submit:  Escape → Enter (vim modal)
```

| Element | Pattern |
|---------|---------|
| Banner | Diamond logo + `Gemini CLI vX.Y.Z` |
| Prompt | `>` between `▀▀▀` / `▄▄▄` bars |
| Mode | `[INSERT]` for typing, `[NORMAL]` for submit |
| Assistant text | `✦ text` |
| Status bar | `workspace · sandbox · model` in bottom row |

**Note:** Gemini uses vim-like modal input. The parser automatically sends `Escape` then `Enter` to submit (configured via `submit_keys` in the profile).

## Orchestrating with Claude Code

The `tmuxai` binary turns the daemon into an orchestration substrate: Claude
Code (or any script) fans prompts out to kiro/gemini/claude/codex subagents
running in tmux, in parallel.

```bash
# Spawn subagents (commands come from each profile's launch_command)
tmuxai spawn w1 --profile kiro-cli      # kiro-cli chat --classic … --model claude-opus-4.6
tmuxai spawn w2 --profile gemini-cli    # gemini -m gemini-3.5-flash --approval-mode yolo …
tmuxai spawn w3 --profile codex-cli     # codex … -m gpt-5.5 -c model_reasoning_effort=xhigh (YOLO)

# Fan out without blocking, then wait for all
tmuxai send w1 "review the auth module" --async
tmuxai send w2 "write tests for the parser" --async
tmuxai wait w1 w2 w3 --timeout 300

# Grab just the answers
tmuxai text w1
tmuxai text w2

# Or run a whole workload over a pool (mix worker types)
tmuxai fanout --workers kiro:2,gemini:2,codex:1 --tasks tasks.json --out results/
```

States returned by `poll`/`wait`: `busy`, `ready`, `question`, `timeout`
(exit code 2), `dead`.

`tmuxai text` refuses (exit 2) while a session is `busy` so a freshly-sent
prompt can't hand back the *previous* turn's answer — `wait` first, then `text`
(or pass `--stale` to read the snapshot anyway). After a send, `poll`/`wait`
report `busy` until the worker is actually observed working, closing the
false-ready race where the pane still shows the prior idle prompt.

A worker can hit `question` even under YOLO — auto-approve suppresses *tool*
prompts, not a model's own clarifying question. See what it's asking with
`tmuxai question <name>`, then `tmuxai answer <name> y Enter` (or the real keys).

Idle sessions are reaped after 30 min (`TMUX_AI_IDLE_REAP_SECS`, `0` disables)
so orphaned fan-out workers don't pile up; `tmuxai ls` shows what's alive.

Watch the whole fleet live with `tmuxai watch` (a refreshing grid of every
session's state + last line) — run it in a *separate* terminal/tmux pane, not
the session driving the fleet; `--once` prints a single frame. Stream one
worker's raw pane with `tmuxai logs <name> -f`.

Subagents run with `--trust-all-tools` / `--approval-mode yolo` (codex: full
YOLO bypass) — point them at a worktree or scratch directory, never at a
checkout with uncommitted work.

**Codex caveat:** codex shows a one-time directory-trust gate the daemon
cannot dismiss. Spawn codex workers with `--cwd <dir>` pointing at a directory
(or git root) already trusted in `~/.codex/config.toml`; otherwise the session
hangs at the trust prompt. Accepting it once for a project persists the trust.

## Architecture

```
┌─────────────────────────────────────────────────┐
│  tmux-ai-parser (library crate)                 │
│                                                 │
│  ┌──────────────┐  ┌────────────────────────┐   │
│  │ Core Engine   │  │ Recognizer Profiles    │   │
│  │              │  │                        │   │
│  │ - Classifier │  │ claude-code.toml       │   │
│  │ - State FSM  │  │ codex-cli.toml         │   │
│  │ - Questions  │  │ kiro-cli.toml          │   │
│  │ - LLM learn  │  │ gemini-cli.toml        │   │
│  └──────────────┘  └────────────────────────┘   │
│                                                 │
│  Output: Vec<Event>                             │
├─────────────────────────────────────────────────┤
│  tmux-ai-io (async I/O crate)                   │
│                                                 │
│  - TmuxSession (spawn/send/capture/kill)        │
│  - ByteWatcher (pipe-pane idle detection)       │
│  - Human input (keystroke simulation)           │
├─────────────────────────────────────────────────┤
│  tmux-ai-daemon (binary)                        │
│                                                 │
│  - Unix socket server (JSON lines)              │
│  - Multi-session management                     │
│  - Auto profile detection                       │
└─────────────────────────────────────────────────┘
```

## Events

The parser emits these event types:

| Event | Description |
|-------|-------------|
| `Ready` | Empty prompt — waiting for input |
| `AssistantText { text }` | AI response text |
| `Question { text, choices }` | Multiple choice question (A/B/C) |
| `ToolUse { tool, args }` | Tool invocation |
| `ToolResult { content }` | Tool output |
| `Thinking { label, elapsed_secs }` | Thinking/processing indicator |
| `SkillLoaded { name }` | Skill loaded |
| `Checklist { tasks }` | Task list with done/active/pending |
| `StatusBar { model, cost, tokens_in, tokens_out, context_pct }` | Status info |
| `StateChange { from, to }` | FSM state transition |
| `UnrecognizedBlock { raw }` | Unknown output (triggers LLM fallback) |
| `Error { message }` | Error detected |

## Human Input Simulation

```rust
use tmux_ai_io::human_input::TypingProfile;

// Default: ~45ms/key, occasional typos + thinking pauses
session.send_human("fix the login bug", &TypingProfile::default()).await?;

// Fast: ~25ms/key, no typos
session.send_human("option A", &TypingProfile::fast()).await?;

// Slow: ~80ms/key, more typos + longer pauses
session.send_human("let me think about this", &TypingProfile::slow()).await?;

// Custom
session.send_human("hello", &TypingProfile {
    base_delay_ms: 60,
    jitter_ms: 40,
    pause_chance: 0.04,
    pause_ms: 500,
    typo_chance: 0.01,
}).await?;
```

Features:
- Per-character send via `tmux send-keys -l`
- Random delay jitter between keystrokes
- Longer pauses after spaces and punctuation
- Occasional typos with backspace correction (nearby-key errors)
- Random thinking pauses mid-typing
- Brief pause before submit

## Self-Updating Parser

When the parser encounters unrecognized output:

1. **Fast path**: Check regex patterns from profile + learned patterns (SQLite)
2. **LLM fallback**: Accumulate unrecognized blocks for 3s, send to LLM API for classification
3. **Pattern promotion**: After 3 consistent LLM classifications, persist regex to SQLite → becomes fast path

```rust
use tmux_ai_parser::learner::PatternStore;

let store = PatternStore::open("learned.db".as_ref())?;
parser.load_learned_patterns(&store)?;

// After parsing, drain unrecognized blocks for LLM classification
let blocks = parser.drain_unrecognized();
// ... send to LLM, get classifications ...
// store.record("claude-code", "banner", r"^╭───", 0.95)?;
// store.promote(3)?; // promote patterns seen 3+ times
```

## Writing a New Profile

Create `profiles/your-cli.toml`:

```toml
[meta]
name = "your-cli"
cli_command = "your-cli"
banner = 'YourCLI v[\d.]+'
version_capture = 'v([\d.]+)'

[prompt]
pattern = '^>\s*$'           # empty prompt regex
input = '^> (.+)'            # prompt with user input
submit_keys = ["Enter"]      # or ["Escape", "Enter"] for vim-like

[separator]
pattern = '^─{20,}'

[markers]
assistant_prefix = "▶"       # what prefixes AI responses
tool_use = '^▶ Run (.+)'     # tool invocation pattern
tool_result = '^\s+(.+)'     # tool output
truncated = ''                # leave empty if N/A
thinking = '^⏳ (.+)'
skill_load = ''

[tasks]
done = "✓"
active = "●"
pending = "○"

[status_bar]
pattern = ''                  # leave empty if no status bar
peak_indicator = ''
memory = ''

[question]
choice_pattern = '^\s*([A-Z])\)\s+(.+)'

[error]
patterns = ['^Error:', '^✗']
```

Then test with a real capture:

```bash
# Capture real output
tmux new-session -d -s test "your-cli"
sleep 5
tmux send-keys -t test "hello" Enter
sleep 10
tmux capture-pane -t test -p -S -50 > fixture.txt
tmux kill-session -t test
```

## Development

```bash
cargo build              # build all crates
cargo test               # run all tests
cargo run --example live_session  # live demo with Claude Code

# Run the daemon
TMUX_AI_PROFILES=profiles cargo run --bin tmux-ai-daemon
```
