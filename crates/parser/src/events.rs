use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Idle,
    Thinking,
    ToolUse,
    ToolResult,
    Responding,
    WaitingForInput,
    Asking,
    Checklist,
    Error,
}

impl Default for State {
    fn default() -> Self { State::Idle }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    StateChange { from: State, to: State },
    AssistantText { text: String, is_complete: bool },
    ToolUse { tool: String, args: String },
    ToolResult { content: String, truncated_lines: Option<u32> },
    Question { text: String, choices: Vec<Choice> },
    SkillLoaded { name: String },
    Thinking { label: String, elapsed_secs: Option<f64> },
    Checklist { tasks: Vec<Task> },
    StatusBar(StatusBarInfo),
    UnrecognizedBlock { raw: String },
    Error { message: String },
    Ready,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub key: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub status: TaskStatus,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus { Done, Active, Pending }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusBarInfo {
    pub model: String,
    pub cost: Option<f64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub context_pct: Option<f32>,
    pub peak_status: Option<String>,
    pub memory_pct: Option<u32>,
}
