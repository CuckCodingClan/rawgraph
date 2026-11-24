//! Transient status-bar messages.

use std::time::{Duration, Instant};

const SHOW_FOR: Duration = Duration::from_secs(6);

#[derive(Default)]
pub struct Status {
    notice: Option<Notice>,
}

struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

impl Status {
    pub fn info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: false, at: Instant::now() });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: true, at: Instant::now() });
    }

    /// The message to show right now: `(text, is_error)`.
    pub fn current(&self) -> Option<(&str, bool)> {
        self.notice.as_ref().filter(|n| n.at.elapsed() < SHOW_FOR).map(|n| (n.text.as_str(), n.error))
    }

    /// True while a message is visible (the UI keeps repainting to expire it).
    pub fn active(&self) -> bool {
        self.current().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_message_wins() {
        let mut s = Status::default();
        assert!(s.current().is_none());
        s.info("a");
        s.error("b");
        assert_eq!(s.current(), Some(("b", true)));
    }
}
