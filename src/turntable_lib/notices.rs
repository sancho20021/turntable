//! Warnings for the TUI, and the conditions that outlast them.
//!
//! A notice is a moment - an action refused, a problem that has happened - and
//! fades once it has been read. A standing notice is a condition that is still
//! true, such as a dead scanner, and holds the panel whenever no fresh notice
//! covers it.

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

/// Holds one notice of each kind: whichever happened last, and whatever is
/// still wrong.
#[derive(Clone)]
pub struct Notices {
    latest: Arc<RwLock<Option<(Notice, Instant)>>>,
    standing: Arc<RwLock<Option<Notice>>>,
}

impl Notices {
    pub fn new() -> Self {
        Self {
            latest: Arc::new(RwLock::new(None)),
            standing: Arc::new(RwLock::new(None)),
        }
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.raise(Level::Warning, message);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.raise(Level::Error, message);
    }

    /// Something that stays broken, shown for the rest of the run.
    pub fn standing(&self, message: impl Into<String>) {
        let notice = Notice {
            message: message.into(),
            level: Level::Error,
        };

        match self.standing.write() {
            Ok(mut standing) => *standing = Some(notice),
            Err(_) => {
                log::error!("cannot raise a standing notice, lock poisoned (tui may be dead)")
            }
        }
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

    /// What to show, if anything. A fresh notice covers a standing one.
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
            .or_else(|| {
                self.standing
                    .read()
                    .ok()
                    .and_then(|standing| standing.clone())
            })
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

    #[test]
    fn a_standing_notice_is_shown_when_nothing_else_is() {
        let notices = Notices::new();
        notices.standing("QR scanner disconnected");

        let current = notices.current().expect("the condition is still true");
        assert_eq!(current.message, "QR scanner disconnected");
        assert_eq!(current.level, Level::Error);
    }

    /// The condition is still true once the notice covering it has gone, so it
    /// has to come back.
    #[test]
    fn a_fresh_notice_covers_a_standing_one_until_it_expires() {
        let notices = Notices::new();
        notices.standing("QR scanner disconnected");
        notices.warn("Could not read that card, scan it again");

        assert_eq!(
            notices.current().expect("the warning is fresh").message,
            "Could not read that card, scan it again"
        );

        if let Ok(mut latest) = notices.latest.write() {
            if let Some((_, raised)) = latest.as_mut() {
                *raised = Instant::now() - LIFETIME - Duration::from_secs(1);
            }
        }

        assert_eq!(
            notices
                .current()
                .expect("the condition is still true")
                .message,
            "QR scanner disconnected"
        );
    }
}
