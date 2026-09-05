//! Passing warnings for the TUI.
//!
//! Carries only what is not a state: an action that was refused, a problem that
//! has already happened. Anything with a lasting state has a panel of its own,
//! and a notice fades once it has been read.

use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

/// How long a notice stays on screen. Long enough to notice and read mid-set,
/// short enough that a stale one is never mistaken for the current situation.
const LIFETIME: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Something was refused, and the DJ can act differently.
    Warning,
    /// Something is broken.
    Error,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub message: String,
    pub level: Level,
}

/// Holds at most one notice: whichever happened last is the one worth showing.
#[derive(Clone)]
pub struct Notices {
    latest: Arc<RwLock<Option<(Notice, Instant)>>>,
}

impl Notices {
    pub fn new() -> Self {
        Self {
            latest: Arc::new(RwLock::new(None)),
        }
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.raise(Level::Warning, message);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.raise(Level::Error, message);
    }

    fn raise(&self, level: Level, message: impl Into<String>) {
        let notice = Notice {
            message: message.into(),
            level,
        };

        match self.latest.write() {
            Ok(mut latest) => *latest = Some((notice, Instant::now())),
            Err(_) => log::error!("cannot raise a notice, lock poisoned (tui may be dead)"),
        }
    }

    /// What to show, if anything.
    pub fn current(&self) -> Option<Notice> {
        let Ok(latest) = self.latest.read() else {
            return Some(Notice {
                message: "notice lock poisoned, a thread may be dead".to_string(),
                level: Level::Error,
            });
        };

        latest
            .as_ref()
            .filter(|(_, raised)| raised.elapsed() < LIFETIME)
            .map(|(notice, _)| notice.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_shown_until_something_happens() {
        assert!(Notices::new().current().is_none());
    }

    #[test]
    fn the_latest_notice_replaces_the_one_before_it() {
        let notices = Notices::new();
        notices.warn("first");
        notices.error("second");

        let current = notices.current().expect("a notice was raised");
        assert_eq!(current.message, "second");
        assert_eq!(current.level, Level::Error);
    }

    /// A notice describes a moment, so it must not outlive its usefulness and
    /// read as the current situation.
    #[test]
    fn a_notice_expires() {
        let notices = Notices::new();
        notices.warn("stale");

        if let Ok(mut latest) = notices.latest.write() {
            if let Some((_, raised)) = latest.as_mut() {
                *raised = Instant::now() - LIFETIME - Duration::from_secs(1);
            }
        }

        assert!(notices.current().is_none());
    }
}
