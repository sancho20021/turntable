//! Staging for the QR gun, so a scan is pulled rather than pushed.
//!
//! The gun may re-read the same card due to a shift in lighting / angle / etc.
//! To prevent re-loads of the same tracks, the scans are absorbed here and the
//! newest one is handed over only when something asks.
//!
//! A gun that has failed serves nothing.

use std::{
    fmt,
    io::ErrorKind,
    ops::ControlFlow,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, RecvTimeoutError, Sender};
use localdeck_qr_scanner::{
    PortOpenError, QrScanner, ScanEvent, ScannerStopped, extract_cardid, start_qr_scanner,
};

use crate::input_event::{AppEvent, InputEvent};
use crate::notices::Notices;
use crate::record::TrackRef;

/// How long the staging thread blocks before checking whether the app is stopping.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct ScannedCard {
    /// exactly what the gun read, unparsed
    pub payload: String,
    /// When the gun last read it.
    pub at: Instant,
    pub outcome: Outcome,
}

/// How far a scanned card got towards being playable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The lookup is running. Published before it starts, since it reads the
    /// library off USB and can take seconds if the drive has spun down.
    Resolving,
    /// Handed to the tray. What happens to it from there is the tray's to report.
    SentToTray,
    /// The library has no such card.
    Unknown,
    Failed(String),
}

/// A card's track, if the library has one.
pub trait CardResolver: Send {
    fn resolve(&mut self, card_id: &str) -> Result<TrackRef, ResolveError>;
}

#[derive(Debug, Clone)]
pub enum ResolveError {
    /// No such card in the library.
    Unknown,
    /// The card is known but its track could not be produced.
    Failed(String),
}

/// Why the gun cannot be trusted right now.
#[derive(Debug, Clone)]
pub enum ScannerFault {
    Disconnected(String),
    Faulted(String),
}

impl fmt::Display for ScannerFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScannerFault::Disconnected(message) => write!(f, "DISCONNECTED: {message}"),
            ScannerFault::Faulted(message) => write!(f, "FAULTED: {message}"),
        }
    }
}

/// What the reader will serve, shaped so a caller cannot reach a card without
/// having considered a dead gun.
///
/// A fault hides the card: a gun that has stopped working may have missed a card
/// being put down, so the last scan is no longer known to be the one in the DJ's
/// hand.
#[derive(Debug, Clone)]
pub enum Staged {
    Card(ScannedCard),
    /// The gun is answering and has read nothing yet.
    Empty,
    Unavailable(ScannerFault),
}

/// Owns the gun and the thread draining it.
pub struct CardReader {
    staged: Arc<RwLock<Staged>>,
    scanner: QrScanner,
    staging: JoinHandle<()>,
}

/// Read access for the threads that display or consume scans, separate from the
/// handle that owns the gun's lifetime.
#[derive(Clone)]
pub struct CardReaderView {
    staged: Arc<RwLock<Staged>>,
}

impl CardReaderView {
    pub fn staged(&self) -> Staged {
        match self.staged.read() {
            Ok(staged) => staged.clone(),
            Err(_) => Staged::Unavailable(ScannerFault::Faulted(
                "card reader lock poisoned, its thread may be dead".to_string(),
            )),
        }
    }
}

/// Opens the gun and starts draining it.
///
/// The port is opened before the thread starts, so a caller that cannot run
/// without a scanner can refuse to start rather than discovering it later.
pub fn start(
    shutdown: Arc<AtomicBool>,
    resolver: Box<dyn CardResolver>,
    tracks: Sender<InputEvent>,
    notices: Notices,
) -> Result<CardReader, PortOpenError> {
    let (events, scanner) = start_qr_scanner()?;
    let staged = Arc::new(RwLock::new(Staged::Empty));

    let staging = {
        let staged = Arc::clone(&staged);
        std::thread::spawn(move || stage_scans(events, staged, shutdown, resolver, tracks, notices))
    };

    Ok(CardReader {
        staged,
        scanner,
        staging,
    })
}

impl CardReader {
    pub fn view(&self) -> CardReaderView {
        CardReaderView {
            staged: Arc::clone(&self.staged),
        }
    }

    /// Closes the port and joins both threads. Takes up to a second, which is
    /// how long the gun's read timeout can hold its thread.
    pub fn stop(self) {
        self.scanner.shutdown();
        if self.staging.join().is_err() {
            log::error!("Card reader staging thread panicked");
        }
    }
}

/// Keeps the newest scan, discarding everything the gun read before it.
fn stage_scans(
    events: Receiver<ScanEvent>,
    staged: Arc<RwLock<Staged>>,
    shutdown: Arc<AtomicBool>,
    mut resolver: Box<dyn CardResolver>,
    tracks: Sender<InputEvent>,
    notices: Notices,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match events.recv_timeout(POLL_INTERVAL) {
            Ok(event) => {
                if stage_scan(event, &staged, resolver.as_mut(), &tracks, &notices).is_break() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            // On the way out the gun's thread is stopped first, so a disconnect
            // there is the expected end rather than something to report.
            Err(RecvTimeoutError::Disconnected) => {
                if !shutdown.load(Ordering::Relaxed) {
                    let fault = ScannerFault::Faulted("scanner thread stopped".to_string());
                    raise(&staged, &notices, fault);
                }
                break;
            }
        }
    }

    log::debug!("Card reader staging stopped");
}

/// Applies one message from the gun and says whether to keep reading. A fault
/// ends the loop, because the gun's own thread exits after reporting one.
fn stage_scan(
    event: ScanEvent,
    staged: &RwLock<Staged>,
    resolver: &mut dyn CardResolver,
    tracks: &Sender<InputEvent>,
    notices: &Notices,
) -> ControlFlow<()> {
    let payload = match event {
        ScanEvent::Scan(payload) => payload,
        ScanEvent::Unreadable(why) => {
            forget_the_card(staged, notices, &why);
            return ControlFlow::Continue(());
        }
        ScanEvent::Stopped(error) => {
            raise(staged, notices, fault_of(error));
            return ControlFlow::Break(());
        }
    };

    let at = Instant::now();

    if was_already_staged(staged, &payload, at) {
        return ControlFlow::Continue(());
    }

    log::info!("qr scan staged: {payload}");
    publish(
        staged,
        Staged::Card(ScannedCard {
            payload: payload.clone(),
            at,
            outcome: Outcome::Resolving,
        }),
    );

    // Reads the library off the USB drive, so it takes seconds if the drive has
    // spun down.
    let outcome = match resolver.resolve(&extract_cardid(&payload)) {
        Ok(track) => send_to_tray(tracks, track),
        Err(ResolveError::Unknown) => {
            log::warn!("card not in the library: {payload}");
            Outcome::Unknown
        }
        Err(ResolveError::Failed(reason)) => {
            log::error!("cannot play card {payload}: {reason}");
            Outcome::Failed(reason)
        }
    };

    publish(
        staged,
        Staged::Card(ScannedCard {
            payload,
            at,
            outcome,
        }),
    );
    ControlFlow::Continue(())
}

/// Whether the gun re-read the card already staged, in which case its timestamp
/// is moved to `at` and nothing else changes.
fn was_already_staged(staged: &RwLock<Staged>, payload: &str, at: Instant) -> bool {
    let Ok(mut slot) = staged.write() else {
        log::error!("cannot update the staged card, lock poisoned");
        return false;
    };

    match &mut *slot {
        Staged::Card(card) if card.payload == payload => {
            card.at = at;
            true
        }
        _ => false,
    }
}

fn send_to_tray(tracks: &Sender<InputEvent>, track: TrackRef) -> Outcome {
    log::info!("card resolved to {}", track.path);

    match tracks.try_send(InputEvent::App(AppEvent::PrepareRecord(track))) {
        Ok(()) => Outcome::SentToTray,
        Err(e) => {
            log::error!("cannot reach the record tray: {e}");
            Outcome::Failed(format!("cannot reach the record tray: {e}"))
        }
    }
}

/// Drops whatever is staged, because a card the gun could not read is still a
/// card put down: what is staged is no longer known to be the one in the DJ's
/// hand.
fn forget_the_card(staged: &RwLock<Staged>, notices: &Notices, why: &str) {
    log::warn!("unreadable scan: {why}");
    notices.warn("Could not read that card, scan it again");
    publish(staged, Staged::Empty);
}

/// A dead gun is worth saying twice: the panel carries it for as long as it is
/// true, and the notice catches the eye at the moment it happens.
fn raise(staged: &RwLock<Staged>, notices: &Notices, fault: ScannerFault) {
    log::error!("qr scanner unusable: {fault}");

    notices.error(match &fault {
        ScannerFault::Disconnected(reason) => format!("QR scanner disconnected: {reason}"),
        ScannerFault::Faulted(reason) => format!("QR scanner stopped working: {reason}"),
    });

    publish(staged, Staged::Unavailable(fault));
}

fn publish(staged: &RwLock<Staged>, value: Staged) {
    match staged.write() {
        Ok(mut slot) => *slot = value,
        Err(_) => log::error!("cannot update the staged card, lock poisoned"),
    }
}

/// Separates a gun that has left the bus from one that is still there and
/// misbehaving, which are different things to tell the user.
fn fault_of(stopped: ScannerStopped) -> ScannerFault {
    let ScannerStopped { kind, message } = stopped;

    match kind {
        ErrorKind::NotFound
        | ErrorKind::NotConnected
        | ErrorKind::BrokenPipe
        | ErrorKind::ConnectionAborted
        | ErrorKind::UnexpectedEof => ScannerFault::Disconnected(message),
        _ => ScannerFault::Faulted(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam::channel::bounded;

    /// Answers every card with the same track, and counts how often it was asked.
    struct Library {
        answer: Result<TrackRef, ResolveError>,
        lookups: usize,
    }

    impl Library {
        fn holding_everything() -> Self {
            Self {
                answer: Ok(TrackRef {
                    path: "/music/track.flac".to_string(),
                    meta: None,
                }),
                lookups: 0,
            }
        }

        fn holding_nothing() -> Self {
            Self {
                answer: Err(ResolveError::Unknown),
                lookups: 0,
            }
        }
    }

    impl CardResolver for Library {
        fn resolve(&mut self, _card_id: &str) -> Result<TrackRef, ResolveError> {
            self.lookups += 1;
            self.answer.clone()
        }
    }

    /// One run of the staging logic: no port, no threads, no database. Reports
    /// what the reader ends up serving, what reached the tray, and how many
    /// lookups it took to get there.
    fn run(events: Vec<ScanEvent>, mut library: Library) -> Run {
        let staged = Arc::new(RwLock::new(Staged::Empty));
        let (tracks, prepared) = bounded(16);
        let notices = Notices::new();

        for event in events {
            if stage_scan(event, &staged, &mut library, &tracks, &notices).is_break() {
                break;
            }
        }

        Run {
            staged: CardReaderView { staged }.staged(),
            prepared: prepared.try_iter().count(),
            lookups: library.lookups,
        }
    }

    struct Run {
        staged: Staged,
        /// `PrepareRecord` events that reached the tray.
        prepared: usize,
        lookups: usize,
    }

    fn scan(payload: &str) -> ScanEvent {
        ScanEvent::Scan(payload.to_string())
    }

    /// A card the gun saw and could not make out.
    fn unreadable() -> ScanEvent {
        ScanEvent::Unreadable("stream did not contain valid UTF-8".to_string())
    }

    fn scanner_stopped(kind: ErrorKind) -> ScannerStopped {
        ScannerStopped {
            kind,
            message: "device went away".to_string(),
        }
    }

    fn card_of(staged: Staged) -> ScannedCard {
        match staged {
            Staged::Card(card) => card,
            other => panic!("expected a staged card, got {other:?}"),
        }
    }

    fn payload_of(staged: Staged) -> String {
        card_of(staged).payload
    }

    /// The whole point of the module: a card left in front of the gun is read
    /// over and over, and must be looked up and sent to the tray exactly once.
    #[test]
    fn a_card_read_fifty_times_is_prepared_once() {
        let scans = (0..50).map(|_| scan("1701")).collect();
        let run = run(scans, Library::holding_everything());

        assert_eq!(run.lookups, 1, "the library was asked more than once");
        assert_eq!(
            run.prepared, 1,
            "the track was sent to the tray more than once"
        );
        assert_eq!(payload_of(run.staged), "1701");
    }

    #[test]
    fn a_different_card_is_looked_up_and_sent() {
        let scans = vec![scan("1701"), scan("1701"), scan("42")];
        let run = run(scans, Library::holding_everything());

        assert_eq!(run.lookups, 2);
        assert_eq!(run.prepared, 2);
        assert_eq!(payload_of(run.staged), "42");
    }

    /// Anything with a QR code on it can end up in front of the gun, so a card
    /// the library does not have must leave the tray alone.
    #[test]
    fn an_unknown_card_reaches_the_tray_as_nothing() {
        let run = run(vec![scan("a wifi code")], Library::holding_nothing());

        assert_eq!(run.prepared, 0, "an unknown card was sent to the tray");
        assert_eq!(card_of(run.staged).outcome, Outcome::Unknown);
    }

    #[test]
    fn a_resolved_card_reports_reaching_the_tray() {
        let run = run(vec![scan("1701")], Library::holding_everything());
        assert_eq!(card_of(run.staged).outcome, Outcome::SentToTray);
    }

    /// A card put down that the gun never read must not serve whatever was
    /// scanned before it. Serving the older card is the failure that plays the
    /// wrong track.
    #[test]
    fn a_fault_serves_nothing_rather_than_the_last_card() {
        let run = run(
            vec![
                scan("1701"),
                ScanEvent::Stopped(scanner_stopped(ErrorKind::BrokenPipe)),
            ],
            Library::holding_everything(),
        );

        assert!(
            matches!(run.staged, Staged::Unavailable(_)),
            "a broken scanner offered {:?}",
            run.staged
        );
    }

    #[test]
    fn nothing_is_staged_before_the_first_scan() {
        assert!(matches!(
            run(vec![], Library::holding_everything()).staged,
            Staged::Empty
        ));
    }

    /// A card the gun could not make out was still a card put down. Serving the
    /// one before it is how the wrong track gets played.
    #[test]
    fn an_unreadable_scan_clears_the_card_without_killing_the_gun() {
        let run = run(
            vec![scan("1701"), unreadable()],
            Library::holding_everything(),
        );

        assert!(
            matches!(run.staged, Staged::Empty),
            "an unreadable scan left {:?} staged",
            run.staged
        );
    }

    /// The gun is still reading, so the next card must go through as normal.
    #[test]
    fn a_card_after_an_unreadable_scan_is_still_played() {
        let run = run(
            vec![unreadable(), scan("42")],
            Library::holding_everything(),
        );

        assert_eq!(run.prepared, 1, "the card after a bad scan never played");
        assert_eq!(payload_of(run.staged), "42");
    }

    /// The TUI says which of the two happened, so they must not collapse.
    #[test]
    fn a_vanished_gun_reads_differently_from_a_misbehaving_one() {
        assert!(matches!(
            fault_of(scanner_stopped(ErrorKind::NotConnected)),
            ScannerFault::Disconnected(_)
        ));
        assert!(matches!(
            fault_of(scanner_stopped(ErrorKind::PermissionDenied)),
            ScannerFault::Faulted(_)
        ));
    }
}
