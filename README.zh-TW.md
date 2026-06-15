# tmux-ai-parser — AI CLI 終端輸出解析引擎

把 AI CLI 工具（Claude Code、Codex CLI、Kiro CLI、Gemini CLI）的終端輸出，解析成結構化事件。

讓你的程式可以「看懂」這些 AI 在 terminal 裡做了什麼。

## 為什麼需要這個？

| | 直接用 API | 用 `-p` 模式 | 用 tmux + 這個引擎 |
|---|---|---|---|
| Skills（brainstorming、TDD） | ❌ | ❌ | ✅ |
| 完整工具（bash、edit、read） | ❌ 要自己做 | ✅ 但有限 | ✅ 完整 |
| Codebase context | ❌ 要自己塞 | ❌ | ✅ CLAUDE.md + memory |
| 計費 | API 定價 | extra usage | **訂閱內用量** |
| 多 AI 支援 | 一次一個 | 一次一個 | Claude + Codex + Kiro + Gemini |

## 快速開始

### 當 library 用

```rust
use tmux_ai_io::TmuxSession;
use tmux_ai_parser::{Parser, events::Event};
use tmux_ai_parser::profile::CompiledProfile;

let profile = CompiledProfile::load("profiles/claude-code.toml".as_ref())?;
let mut parser = Parser::new(profile);
let mut session = TmuxSession::spawn("my-session", "claude --dangerously-skip-permissions", ".").await?;

// 用人類打字速度送訊息
session.send_human("修好 checkout.ts 的 null check bug", &TypingProfile::default()).await?;

// 等回應、解析事件
let events = session.wait_and_capture().await.map(|t| parser.parse_snapshot(&t))?;
for event in events {
    match event {
        Event::AssistantText { text, .. } => println!("回應: {text}"),
        Event::Question { choices, .. } => { /* 多選題 */ },
        Event::Ready => { /* 可以送下一個訊息了 */ },
        _ => {}
    }
}
```

### 當 daemon 用

```bash
# 啟動 daemon
TMUX_AI_PROFILES=profiles tmux-ai-daemon

# 用 Unix socket 操作
echo '{"create_session":{"name":"demo","command":"claude --dangerously-skip-permissions","cwd":"."}}' | nc -U /tmp/tmux-ai-parser.sock
echo '{"send_input":{"session":"demo","text":"say hello"}}' | nc -U /tmp/tmux-ai-parser.sock
```

## 支援的 AI CLI

| CLI | 版本 | Prompt | 回應標記 | 提交方式 |
|-----|------|--------|---------|---------|
| Claude Code | v2.1.92 | `❯` | `⏺ text` | Enter |
| Codex CLI | v0.118.0 | `›` | `• text` | Enter |
| Kiro CLI | v1.28.2 | `N% λ >` | `> text` | Enter |
| Gemini CLI | v0.36.0 | `>` (vim) | `✦ text` | Esc → Enter |

每個 CLI 的輸出格式完全不同 — 這就是為什麼需要 profile-driven 設計。

## 架構

```
┌─────────────────────────────────────────────────┐
│  tmux-ai-parser（核心 library）                   │
│                                                 │
│  ┌──────────────┐  ┌────────────────────────┐   │
│  │ 解析引擎      │  │ 辨識 Profile            │   │
│  │              │  │                        │   │
│  │ - 行分類器    │  │ claude-code.toml       │   │
│  │ - 狀態機      │  │ codex-cli.toml         │   │
│  │ - 問題偵測    │  │ kiro-cli.toml          │   │
│  │ - LLM 自學    │  │ gemini-cli.toml        │   │
│  └──────────────┘  └────────────────────────┘   │
├─────────────────────────────────────────────────┤
│  tmux-ai-io（非同步 I/O）                         │
│                                                 │
│  - TmuxSession（建立/送訊/擷取/關閉）              │
│  - ByteWatcher（pipe-pane 閒置偵測）              │
│  - 人類打字模擬（隨機延遲、偶爾打錯字）              │
├─────────────────────────────────────────────────┤
│  tmux-ai-daemon（共享服務）                        │
│                                                 │
│  - Unix socket（JSON lines 協議）                 │
│  - 多 session 管理                               │
│  - 自動偵測 CLI profile                           │
└─────────────────────────────────────────────────┘
```

## 事件類型

| 事件 | 說明 |
|------|------|
| `Ready` | 空 prompt — 等待輸入 |
| `AssistantText` | AI 的回應文字 |
| `Question` | 多選題（A/B/C） |
| `ToolUse` | 工具呼叫（bash、edit、read） |
| `ToolResult` | 工具輸出 |
| `Thinking` | 思考中指示器 |
| `SkillLoaded` | Skill 載入 |
| `Checklist` | 任務清單（完成/進行中/待辦） |
| `StatusBar` | 模型、費用、token、context % |
| `UnrecognizedBlock` | 未知輸出（觸發 LLM 自學） |

## 自我更新機制

遇到看不懂的輸出時：

1. **快速路徑**：用 profile 的 regex + 已學會的 pattern（SQLite）比對
2. **LLM 後備**：累積 3 秒未知內容，送 LLM API 分類
3. **Pattern 升級**：同樣的分類出現 3 次 → 存入 SQLite → 變成快速路徑

下次 Claude Code 更新 UI 格式，引擎會自動學會新的 pattern。

## 人類打字模擬

```rust
// 預設：~45ms/鍵，偶爾打錯字 + 思考停頓
session.send_human("修好 bug", &TypingProfile::default()).await?;

// 快速：~25ms/鍵，不打錯字
session.send_human("A", &TypingProfile::fast()).await?;

// 慢速：~80ms/鍵，更多錯字 + 更長停頓
session.send_human("讓我仔細想想這個問題", &TypingProfile::slow()).await?;
```

特色：
- 每個字元獨立送出
- 隨機延遲抖動
- 空格和標點後多停一下
- 偶爾打錯字再退格修正（打到旁邊的鍵）
- 隨機思考停頓
- 送出前短暫停頓

## 寫新的 Profile

建立 `profiles/your-cli.toml`：

```toml
[meta]
name = "your-cli"
cli_command = "your-cli"
banner = 'YourCLI v[\d.]+'
version_capture = 'v([\d.]+)'

[prompt]
pattern = '^>\s*$'
input = '^> (.+)'
submit_keys = ["Enter"]      # Gemini 用 ["Escape", "Enter"]

[markers]
assistant_prefix = "▶"
tool_use = '^▶ Run (.+)'
# ... 其他 pattern
```

然後用真實輸出驗證：

```bash
tmux new-session -d -s test "your-cli"
sleep 5
tmux send-keys -t test "hello" Enter
sleep 10
tmux capture-pane -t test -p -S -50 > fixture.txt
```

## 開發

```bash
cargo build              # 建置
cargo test               # 跑測試（9 個）
cargo run --example live_session  # 即時 demo

# 跑 daemon
TMUX_AI_PROFILES=profiles cargo run --bin tmux-ai-daemon
```

## 專案統計

- **2,400+ 行 Rust** + 4 個 TOML profile + 4 個真實 fixture
- **9 個測試**全部通過
- **13 個 commit**，從零到完整可用
- 從 brainstorming 到實作，一個下午完成

## 授權

以 [MIT License](LICENSE) 釋出。
