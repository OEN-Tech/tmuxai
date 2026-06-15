use crate::session::tmux;

/// Capture the current pane content as plain text (ANSI stripped by tmux).
/// `history_lines` controls how far back to capture (e.g., 200).
pub async fn capture_pane(session: &str, history_lines: u32) -> Result<String, String> {
    let scroll = format!("-{history_lines}");
    tmux(&["capture-pane", "-t", session, "-p", "-S", &scroll]).await
}

/// Capture with escape codes (for debugging).
pub async fn capture_pane_raw(session: &str, history_lines: u32) -> Result<String, String> {
    let scroll = format!("-{history_lines}");
    tmux(&["capture-pane", "-t", session, "-p", "-e", "-S", &scroll]).await
}
