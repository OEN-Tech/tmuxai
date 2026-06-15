pub mod session;
pub mod pipe_pane;
pub mod capture;
pub mod send_keys;
pub mod human_input;

use std::time::Duration;
use tmux_ai_parser::{Parser, events::Event};
use tmux_ai_parser::profile::CompiledProfile;
use session::SessionConfig;
use pipe_pane::ByteWatcher;

const DEFAULT_IDLE_THRESHOLD: Duration = Duration::from_millis(1500);
const DEFAULT_STARTUP_WAIT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(120);

/// Extract the LAST editor-mode indicator match from a pane snapshot.
/// Last wins: scrollback can contain stale indicators from earlier renders.
pub fn extract_mode(text: &str, indicator: &regex::Regex) -> Option<String> {
    let mut last = None;
    for cap in indicator.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            last = Some(m.as_str().to_string());
        }
    }
    last
}

pub struct TmuxSession {
    name: String,
    watcher: Option<ByteWatcher>,
    idle_threshold: Duration,
    max_wait: Duration,
    submit_keys: Vec<String>,
    mode_indicator: Option<regex::Regex>,
    mode_insert_key: Option<String>,
}

impl TmuxSession {
    /// Spawn a new tmux session running the given command.
    pub async fn spawn(name: &str, command: &str, cwd: &str) -> Result<Self, String> {
        // Kill any existing session with this name
        let _ = session::kill_session(name).await;

        session::create_session(&SessionConfig {
            name: name.to_string(),
            command: command.to_string(),
            cwd: cwd.to_string(),
            ..Default::default()
        }).await?;

        // Wait for the CLI to start
        tokio::time::sleep(DEFAULT_STARTUP_WAIT).await;

        // Start byte watcher
        let watcher = ByteWatcher::start(name).await?;

        Ok(Self {
            name: name.to_string(),
            watcher: Some(watcher),
            idle_threshold: DEFAULT_IDLE_THRESHOLD,
            max_wait: DEFAULT_MAX_WAIT,
            submit_keys: vec!["Enter".to_string()],
            mode_indicator: None,
            mode_insert_key: None,
        })
    }

    /// Set the submit key sequence (from profile).
    pub fn set_submit_keys(&mut self, keys: Vec<String>) {
        self.submit_keys = keys;
    }

    /// Configure modal-input handling (from profile [modes]).
    pub fn set_modes(&mut self, indicator: Option<regex::Regex>, insert_key: Option<String>) {
        self.mode_indicator = indicator;
        self.mode_insert_key = insert_key;
    }

    /// If the profile declares modes and the pane is not in INSERT,
    /// press the insert key before typing (fixes gemini's NORMAL-mode trap).
    async fn ensure_insert_mode(&self) -> Result<(), String> {
        let (Some(re), Some(key)) = (&self.mode_indicator, &self.mode_insert_key) else {
            return Ok(());
        };
        let text = capture::capture_pane(&self.name, 50).await?;
        if let Some(mode) = extract_mode(&text, re) {
            if mode != "INSERT" {
                send_keys::send_raw_keys(&self.name, key).await?;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        Ok(())
    }

    /// Send text input using the configured submit key sequence.
    pub async fn send(&self, input: &str) -> Result<(), String> {
        self.ensure_insert_mode().await?;
        send_keys::send_keys_with_profile(&self.name, input, &self.submit_keys).await
    }

    /// Send text with human-like keystroke simulation.
    pub async fn send_human(&self, input: &str, typing: &human_input::TypingProfile) -> Result<(), String> {
        self.ensure_insert_mode().await?;
        human_input::send_keys_human(&self.name, input, &self.submit_keys, typing).await
    }

    /// Wait for output to stabilize, then capture the pane content.
    pub async fn wait_and_capture(&mut self) -> Result<String, String> {
        if let Some(ref mut w) = self.watcher {
            let _ = w.wait_idle(self.idle_threshold, self.max_wait).await?;
        } else {
            tokio::time::sleep(self.idle_threshold).await;
        }
        capture::capture_pane(&self.name, 200).await
    }

    /// Send input, wait for response, parse events.
    pub async fn send_and_parse(&mut self, input: &str, parser: &mut Parser) -> Result<Vec<Event>, String> {
        self.send(input).await?;
        let text = self.wait_and_capture().await?;
        Ok(parser.parse_snapshot(&text))
    }

    /// Capture current pane and parse (no input sent).
    pub async fn capture_and_parse(&mut self, parser: &mut Parser) -> Result<Vec<Event>, String> {
        let text = self.wait_and_capture().await?;
        Ok(parser.parse_snapshot(&text))
    }

    /// Auto-detect which profile matches this session's CLI.
    pub async fn detect_profile(&self, profiles: &[CompiledProfile]) -> Result<Option<usize>, String> {
        let text = capture::capture_pane(&self.name, 10).await?;
        Ok(CompiledProfile::detect_from_text(&text, profiles))
    }

    /// Set the idle detection threshold.
    pub fn set_idle_threshold(&mut self, threshold: Duration) {
        self.idle_threshold = threshold;
    }

    /// Cap for blocking waits (default 120s).
    pub fn set_max_wait(&mut self, max_wait: Duration) {
        self.max_wait = max_wait;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Kill the tmux session and clean up.
    pub async fn kill(&mut self) -> Result<(), String> {
        if let Some(w) = self.watcher.take() {
            w.stop().await?;
        }
        session::kill_session(&self.name).await
    }

    /// Reset the idle baseline to now (call after sending input). Defends the
    /// poll path against reading a stale pre-send silence window as idle.
    pub fn note_input_sent(&mut self) {
        if let Some(ref mut w) = self.watcher {
            w.touch();
        }
    }

    /// One non-blocking idle check (true = no new bytes for `threshold`).
    pub async fn check_idle(&mut self, threshold: Duration) -> Result<bool, String> {
        match self.watcher {
            Some(ref mut w) => Ok(w.check_idle(threshold).await),
            None => Ok(true),
        }
    }

    /// Capture the pane immediately, without waiting for idle.
    pub async fn capture_now(&self, history_lines: u32) -> Result<String, String> {
        capture::capture_pane(&self.name, history_lines).await
    }

    /// Send a raw key sequence (e.g. ["y", "Enter"]) — for answering prompts.
    pub async fn send_raw(&self, keys: &[String]) -> Result<(), String> {
        for key in keys {
            send_keys::send_raw_keys(&self.name, key).await?;
        }
        Ok(())
    }

    /// Interrupt the CLI: Escape (close menus/modes) then Ctrl-C.
    pub async fn interrupt(&self) -> Result<(), String> {
        send_keys::send_raw_keys(&self.name, "Escape").await?;
        send_keys::send_raw_keys(&self.name, "C-c").await
    }
}
