use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::time::sleep;
use crate::session::tmux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome { Idle, TimedOut }

pub struct ByteWatcher {
    session: String,
    fifo_path: PathBuf,
    last_size: u64,
    last_byte_at: Instant,
    active: bool,
}

impl ByteWatcher {
    /// Start watching a tmux session via pipe-pane.
    /// Creates a temp file that tmux writes pane output to.
    pub async fn start(session: &str) -> Result<Self, String> {
        let fifo_path = std::env::temp_dir().join(format!("tmux-ai-parser-{session}.pipe"));
        // Clean up any stale file
        let _ = fs::remove_file(&fifo_path).await;
        // Create the output file
        fs::write(&fifo_path, b"").await.map_err(|e| format!("create pipe file: {e}"))?;
        // Tell tmux to pipe pane output to the file
        let path_str = fifo_path.to_string_lossy().to_string();
        tmux(&["pipe-pane", "-t", session, &format!("cat >> {path_str}")]).await?;
        Ok(Self {
            session: session.to_string(),
            fifo_path,
            last_size: 0,
            last_byte_at: Instant::now(),
            active: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(fifo_path: PathBuf) -> Self {
        Self { session: "test".into(), fifo_path, last_size: 0, last_byte_at: Instant::now(), active: false }
    }

    /// Reset the idle baseline to now, as if bytes just arrived. Call right
    /// after sending input so a stale pre-send silence window cannot read as
    /// idle on the next `check_idle` — the worker must produce a fresh 1.5s of
    /// silence before it counts as done (closes the false-ready race).
    pub fn touch(&mut self) {
        self.last_byte_at = Instant::now();
        self.active = true;
    }

    /// One non-blocking idle check. Persists byte counters across calls so
    /// repeated polling accumulates the idle window correctly.
    pub async fn check_idle(&mut self, threshold: Duration) -> bool {
        let current = fs::metadata(&self.fifo_path).await.map(|m| m.len()).unwrap_or(0);
        if current != self.last_size {
            self.last_size = current;
            self.last_byte_at = Instant::now();
            self.active = true;
            return false;
        }
        if self.last_byte_at.elapsed() >= threshold {
            self.active = false;
            true
        } else {
            false
        }
    }

    /// Wait until the pane has been silent for `threshold`, giving up after
    /// `max_wait` (a constantly-redrawing TUI must not hang the caller).
    pub async fn wait_idle(&mut self, threshold: Duration, max_wait: Duration) -> Result<WaitOutcome, String> {
        let poll_interval = Duration::from_millis(200);
        let start = Instant::now();
        loop {
            if self.check_idle(threshold).await {
                return Ok(WaitOutcome::Idle);
            }
            if start.elapsed() >= max_wait {
                return Ok(WaitOutcome::TimedOut);
            }
            sleep(poll_interval).await;
        }
    }

    /// Check if output is currently flowing.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Time since last byte was detected.
    pub fn idle_duration(&self) -> Duration {
        self.last_byte_at.elapsed()
    }

    /// Stop watching and clean up.
    pub async fn stop(&self) -> Result<(), String> {
        // Disable pipe-pane (empty string disables)
        let _ = tmux(&["pipe-pane", "-t", &self.session]).await;
        let _ = fs::remove_file(&self.fifo_path).await;
        Ok(())
    }
}

impl Drop for ByteWatcher {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.fifo_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("byte-watcher-test-{tag}-{}", std::process::id()));
        std::fs::write(&p, b"").unwrap();
        p
    }

    fn append(path: &PathBuf, bytes: &[u8]) {
        std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
    }

    #[tokio::test]
    async fn check_idle_tracks_byte_flow() {
        let path = temp_file("flow");
        let mut w = ByteWatcher::for_test(path.clone());
        assert!(!w.check_idle(Duration::from_millis(100)).await, "just created — not idle yet");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(w.check_idle(Duration::from_millis(100)).await, "no bytes for 150ms");
        append(&path, b"x");
        assert!(!w.check_idle(Duration::from_millis(100)).await, "new bytes arrived");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn wait_idle_times_out_on_constant_output() {
        let path = temp_file("busy");
        let writer_path = path.clone();
        let writer = tokio::spawn(async move {
            for _ in 0..100 {
                std::fs::OpenOptions::new().append(true).open(&writer_path)
                    .unwrap().write_all(b"x").unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let mut w = ByteWatcher::for_test(path.clone());
        let out = w.wait_idle(Duration::from_millis(400), Duration::from_millis(1200)).await.unwrap();
        assert_eq!(out, WaitOutcome::TimedOut);
        writer.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn wait_idle_returns_idle_when_quiet() {
        let path = temp_file("quiet");
        let mut w = ByteWatcher::for_test(path.clone());
        let out = w.wait_idle(Duration::from_millis(100), Duration::from_secs(5)).await.unwrap();
        assert_eq!(out, WaitOutcome::Idle);
        let _ = std::fs::remove_file(&path);
    }
}
