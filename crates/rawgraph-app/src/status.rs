//! Transient status-bar messages.

use std::time::{Duration, Instant};

const SHOW_FOR: Duration = Duration::from_secs(6);

#[derive(Default)]
pub struct Status {
    notice: Option<Notice>,
    /// Migration notes of the last opened older graph; shown in the status bar until dismissed.
    notes: Vec<String>,
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

    /// Keeps the notes of a graph migration for the status bar and announces them once.
    pub fn migration(&mut self, what: &str, notes: Vec<String>) {
        if notes.is_empty() {
            return;
        }
        let n = notes.len();
        self.info(format!("{what}: older graph updated, {n} note{}", if n == 1 { "" } else { "s" }));
        self.notes = notes;
    }

    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    pub fn clear_notes(&mut self) {
        self.notes.clear();
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
