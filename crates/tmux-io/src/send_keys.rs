use crate::session::tmux;
use tokio::time::{sleep, Duration};

/// Send text to a tmux pane using the profile's submit key sequence.
pub async fn send_keys_with_profile(session: &str, text: &str, submit_keys: &[String]) -> Result<(), String> {
    // Send the text
    tmux(&["send-keys", "-t", session, "--", text]).await?;
    // Brief pause so the TUI input handler finishes processing the typed
    // characters before submit keys arrive (e.g., gemini's React-based TUI
    // needs ~50ms to settle after a batch of keystrokes).
    sleep(Duration::from_millis(60)).await;
    // Send each submit key
    for key in submit_keys {
        tmux(&["send-keys", "-t", session, key]).await?;
    }
    Ok(())
}

/// Send text followed by Enter (default for most CLIs).
pub async fn send_keys(session: &str, text: &str) -> Result<(), String> {
    tmux(&["send-keys", "-t", session, "--", text, "Enter"]).await?;
    Ok(())
}

/// Send raw keys without text (for special keys like Escape, C-c).
pub async fn send_raw_keys(session: &str, keys: &str) -> Result<(), String> {
    tmux(&["send-keys", "-t", session, keys]).await?;
    Ok(())
}
