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

use crossbeam::channel::{Receiver, RecvTimeoutError};
use localdeck_qr_scanner::{QrScanner, QrScannerError, start_qr_scanner};

/// How long the staging thread blocks before checking whether the app is stopping.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct ScannedCard {
    /// exactly what the gun read, unparsed
    pub payload: String,
    /// When the gun last read it.
    pub at: Instant,
    /// Whether this card has already been put in the tray. Survives the gun
    /// re-reading the same card, so the panel keeps offering it while the DJ
    /// lines the next one up.
    pub loaded: bool,
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
pub fn start(shutdown: Arc<AtomicBool>) -> Result<CardReader, QrScannerError> {
    let (events, scanner) = start_qr_scanner()?;
    let staged = Arc::new(RwLock::new(Staged::Empty));

    let staging = {
        let staged = Arc::clone(&staged);
        std::thread::spawn(move || stage_scans(events, staged, shutdown))
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
    events: Receiver<Result<String, QrScannerError>>,
    staged: Arc<RwLock<Staged>>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match events.recv_timeout(POLL_INTERVAL) {
            Ok(event) => {
                if stage_scan(event, &staged).is_break() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            // On the way out the gun's thread is stopped first, so a disconnect
            // there is the expected end rather than something to report.
            Err(RecvTimeoutError::Disconnected) => {
                if !shutdown.load(Ordering::Relaxed) {
                    let fault = ScannerFault::Faulted("scanner thread stopped".to_string());
                    log::error!("qr scanner unusable: {fault}");
                    publish(&staged, Staged::Unavailable(fault));
                }
                break;
            }
        }
    }

    log::debug!("Card reader staging stopped");
}

/// Applies one message from the gun and says whether to keep reading. A fault
/// ends the loop, because the gun's own thread exits after reporting one.
fn stage_scan(event: Result<String, QrScannerError>, staged: &RwLock<Staged>) -> ControlFlow<()> {
    let Ok(mut slot) = staged.write() else {
        log::error!("cannot update the staged card, lock poisoned");
        return ControlFlow::Break(());
    };

    match event {
        Ok(payload) => {
            let loaded = match &*slot {
                Staged::Card(previous) if previous.payload == payload => previous.loaded,
                _ => {
                    log::info!("qr scan staged: {payload}");
                    false
                }
            };

            *slot = Staged::Card(ScannedCard {
                payload,
                at: Instant::now(),
                loaded,
            });
            ControlFlow::Continue(())
        }

        Err(error) => {
            let fault = fault_of(error);
            log::error!("qr scanner unusable: {fault}");
            *slot = Staged::Unavailable(fault);
            ControlFlow::Break(())
        }
    }
}

fn publish(staged: &RwLock<Staged>, value: Staged) {
    match staged.write() {
        Ok(mut slot) => *slot = value,
        Err(_) => log::error!("cannot update the staged card, lock poisoned"),
    }
}

/// Separates a gun that has left the bus from one that is still there and
/// misbehaving, which are different things to tell the user.
fn fault_of(error: QrScannerError) -> ScannerFault {
    match error {
        QrScannerError::PortOpen(message) => ScannerFault::Disconnected(message),

        QrScannerError::ReadError { kind, message } => match kind {
            ErrorKind::NotFound
            | ErrorKind::NotConnected
            | ErrorKind::BrokenPipe
            | ErrorKind::ConnectionAborted
            | ErrorKind::UnexpectedEof => ScannerFault::Disconnected(message),
            _ => ScannerFault::Faulted(message),
        },

        QrScannerError::SendError(message) => ScannerFault::Faulted(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds the messages through the staging logic and reports what the gun
    /// would serve afterwards. No port and no threads.
    fn after(events: Vec<Result<String, QrScannerError>>) -> Staged {
        let staged = Arc::new(RwLock::new(Staged::Empty));
        for event in events {
            if stage_scan(event, &staged).is_break() {
                break;
            }
        }
        CardReaderView { staged }.staged()
    }

    fn read_error(kind: ErrorKind) -> QrScannerError {
        QrScannerError::ReadError {
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

    /// The whole point of the module: the gun re-reads a card it can see, and
    /// all of those repeats have to collapse into the one thing on offer.
    #[test]
    fn repeated_scans_of_one_card_stage_it_once() {
        let scans = (0..50).map(|_| Ok("1701".to_string())).collect();
        assert_eq!(payload_of(after(scans)), "1701");
    }

    #[test]
    fn the_newest_scan_wins() {
        let scans = vec![
            Ok("1701".to_string()),
            Ok("1701".to_string()),
            Ok("42".to_string()),
        ];
        assert_eq!(payload_of(after(scans)), "42");
    }

    /// A card put down that the gun never read must not load whatever was
    /// scanned before it. Serving the older card is the one failure that plays
    /// the wrong track.
    #[test]
    fn a_fault_serves_nothing_rather_than_the_last_card() {
        let staged = after(vec![
            Ok("1701".to_string()),
            Err(read_error(ErrorKind::BrokenPipe)),
        ]);

        assert!(
            matches!(staged, Staged::Unavailable(_)),
            "a broken scanner offered {staged:?}"
        );
    }

    #[test]
    fn nothing_is_staged_before_the_first_scan() {
        assert!(matches!(after(vec![]), Staged::Empty));
    }

    /// The DJ lines a card up while the gun re-reads it, and the panel has to
    /// keep saying "ready to load" throughout rather than only on the first read.
    #[test]
    fn re_reading_a_loaded_card_does_not_offer_it_again() {
        let staged = Arc::new(RwLock::new(Staged::Card(ScannedCard {
            payload: "1701".to_string(),
            at: Instant::now(),
            loaded: true,
        })));

        let _ = stage_scan(Ok("1701".to_string()), &staged);

        let card = card_of(CardReaderView { staged }.staged());
        assert!(card.loaded, "a card already in the tray was offered again");
    }

    /// The signal the DJ waits for while adjusting the angle.
    #[test]
    fn a_different_card_is_offered_even_after_one_was_loaded() {
        let staged = Arc::new(RwLock::new(Staged::Card(ScannedCard {
            payload: "1701".to_string(),
            at: Instant::now(),
            loaded: true,
        })));

        let _ = stage_scan(Ok("42".to_string()), &staged);

        let card = card_of(CardReaderView { staged }.staged());
        assert_eq!(card.payload, "42");
        assert!(!card.loaded, "a freshly scanned card was not offered");
    }

    /// The TUI says which of the two happened, so they must not collapse.
    #[test]
    fn a_vanished_gun_reads_differently_from_a_misbehaving_one() {
        assert!(matches!(
            fault_of(read_error(ErrorKind::NotConnected)),
            ScannerFault::Disconnected(_)
        ));
        assert!(matches!(
            fault_of(read_error(ErrorKind::InvalidData)),
            ScannerFault::Faulted(_)
        ));
    }
}
