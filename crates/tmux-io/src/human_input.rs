use crate::session::tmux;
use std::time::Duration;
use tokio::time::sleep;

/// Typing speed profile.
#[derive(Debug, Clone)]
pub struct TypingProfile {
    /// Base delay between keystrokes in ms.
    pub base_delay_ms: u64,
    /// Random jitter added to base delay (0 to jitter_ms).
    pub jitter_ms: u64,
    /// Chance of a brief pause (simulating thinking mid-word), 0.0-1.0.
    pub pause_chance: f64,
    /// Duration of a thinking pause in ms.
    pub pause_ms: u64,
    /// Chance of a typo + backspace correction, 0.0-1.0.
    pub typo_chance: f64,
}

impl Default for TypingProfile {
    fn default() -> Self {
        Self {
            base_delay_ms: 45,
            jitter_ms: 35,
            pause_chance: 0.03,
            pause_ms: 400,
            typo_chance: 0.02,
        }
    }
}

impl TypingProfile {
    pub fn fast() -> Self {
        Self { base_delay_ms: 25, jitter_ms: 20, pause_chance: 0.01, pause_ms: 200, typo_chance: 0.0 }
    }
    pub fn slow() -> Self {
        Self { base_delay_ms: 80, jitter_ms: 60, pause_chance: 0.05, pause_ms: 800, typo_chance: 0.03 }
    }
}

const NEARBY_KEYS: &[(char, &[char])] = &[
    ('a', &['s', 'q', 'w']), ('s', &['a', 'd', 'w']), ('d', &['s', 'f', 'e']),
    ('f', &['d', 'g', 'r']), ('e', &['w', 'r', 'd']), ('r', &['e', 't', 'f']),
    ('t', &['r', 'y', 'g']), ('i', &['u', 'o', 'k']), ('o', &['i', 'p', 'l']),
    ('n', &['b', 'm', 'h']), ('l', &['k', 'o', 'p']),
];

fn nearby_typo(c: char) -> char {
    let lower = c.to_ascii_lowercase();
    for (key, neighbors) in NEARBY_KEYS {
        if *key == lower {
            let idx = rand::random_range(0..neighbors.len());
            let typo = neighbors[idx];
            return if c.is_uppercase() { typo.to_ascii_uppercase() } else { typo };
        }
    }
    c
}

/// Send text to a tmux pane one character at a time with human-like timing.
pub async fn send_keys_human(
    session: &str,
    text: &str,
    submit_keys: &[String],
    profile: &TypingProfile,
) -> Result<(), String> {
    for ch in text.chars() {
        // Maybe insert a typo
        if profile.typo_chance > 0.0 && ch.is_ascii_alphabetic() && rand::random_bool(profile.typo_chance) {
            let wrong = nearby_typo(ch);
            tmux(&["send-keys", "-t", session, "-l", &wrong.to_string()]).await?;
            sleep(Duration::from_millis(profile.base_delay_ms + rand::random_range(0..=profile.jitter_ms))).await;
            tmux(&["send-keys", "-t", session, "BSpace"]).await?;
            sleep(Duration::from_millis(profile.base_delay_ms + rand::random_range(0..=profile.jitter_ms))).await;
        }

        // Send the actual character (-l for literal)
        tmux(&["send-keys", "-t", session, "-l", &ch.to_string()]).await?;

        // Delay
        let mut delay = profile.base_delay_ms + rand::random_range(0..=profile.jitter_ms);
        if ch == ' ' || ch == '.' || ch == ',' || ch == '?' || ch == '!' {
            delay += rand::random_range(20..80);
        }
        if profile.pause_chance > 0.0 && rand::random_bool(profile.pause_chance) {
            delay += profile.pause_ms;
        }
        sleep(Duration::from_millis(delay)).await;
    }

    // Brief pause before submitting
    sleep(Duration::from_millis(150 + rand::random_range(0..200))).await;

    for key in submit_keys {
        tmux(&["send-keys", "-t", session, key]).await?;
        sleep(Duration::from_millis(80 + rand::random_range(0..60))).await;
    }

    Ok(())
}
