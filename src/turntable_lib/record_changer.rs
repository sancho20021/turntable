//! The record tray: stages one track off disk and hands it to a deck on request.
//!
//! Loading is deliberately two gestures, because that is what both input
//! devices want: dragging a file in only *stages* it (`Prepare`), and a later
//! `Commit` says which deck it goes on. In keyboard mode the commit is a key
//! press onto the active deck; a MIDI controller's LOAD buttons name the deck
//! themselves. Nothing here knows which of those is driving it.
//!
//! Three threads:
//!
//! * **tray** — owns the state machine and the staged record, never blocks;
//! * **loader** — blocking `load_file` calls, nothing else;
//! * **disposer** — frees records handed back by the audio thread.
//!
//! The split between the first two is what lets a `Prepare` that arrives
//! mid-decode be rejected as busy *immediately* instead of after the decode it
//! was supposed to be rejected by.

use std::{
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, Sender, select};

use crate::{
    deck_controller::{AppStatus, RecordInfo},
    deck_thread::DeckId,
    deck_worker::DeckWorkerEvent,
    decoder::load_file,
    platter_audio_processor::PlatterAudioProcessor,
    record::{Record, interpolation::Interpolator},
    utils::log_try_send,
};

pub enum RecordChangerCommand {
    /// Stage a track: decode it off disk and hold it until it is committed.
    Prepare { path: String },
    /// Put the staged record on a deck. The tray is empty afterwards.
    Commit { deck_id: DeckId },
}

/// What the tray holds right now.
///
/// Published for the TUI to render; the decoded buffer itself never leaves the
/// tray thread, so reading this never touches tens of megabytes of samples.
#[derive(Debug, Clone)]
pub enum TrayState {
    /// Nothing staged.
    Empty,
    /// Decoding off disk. Further `Prepare`s are rejected until this finishes.
    Decoding { path: String, since: Instant },
    /// Decoded, waiting for a `Commit` to say which deck it goes on.
    Ready { info: RecordInfo },
    /// The last decode failed. The tray holds nothing.
    Failed { path: String, error: String },
}

struct RecordDisposer {
    records: Receiver<Record>,
    shutdown: Arc<AtomicBool>,
}

impl RecordDisposer {
    fn run(self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            let record = match self.records.recv_timeout(Duration::from_millis(100)) {
                Ok(r) => r,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            };
            drop(record);
            log::debug!("Record safely deallocated");
        }
        log::debug!("Record disposer terminated");
    }
}

/// Result of one decode, handed from the loader thread back to the tray.
struct LoadOutcome {
    path: String,
    result: Result<(Record, RecordInfo), String>,
}

/// Blocking decoder. Deliberately dumb: it knows nothing about decks or tray
/// state, which is what keeps the tray thread responsive during a decode.
struct RecordLoader {
    paths: Receiver<String>,
    outcomes: Sender<LoadOutcome>,
    shutdown: Arc<AtomicBool>,
}

impl RecordLoader {
    fn run(self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            let path = match self.paths.recv_timeout(Duration::from_millis(100)) {
                Ok(p) => p,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            };

            let result = match load_file(Path::new(&path)) {
                Ok(samples) => {
                    let samples_n = samples.len();
                    Ok((
                        Record::new(samples, Interpolator::linear()),
                        RecordInfo {
                            path: path.clone(),
                            duration: PlatterAudioProcessor::frames_to_dur_nanos(samples_n),
                        },
                    ))
                }
                Err(e) => Err(e.to_string()),
            };

            if self.outcomes.send(LoadOutcome { path, result }).is_err() {
                break;
            }
        }
        log::debug!("Record loader terminated");
    }
}

/// Owner of the staged record and of the state machine over it.
struct Tray<const DECKS: usize> {
    /// The decoded record, present exactly while `state` is [`TrayState::Ready`].
    staged: Option<(Record, RecordInfo)>,
    /// Authoritative state; mirrored into `published` on every change.
    state: TrayState,
    published: Arc<RwLock<TrayState>>,
    loader: Sender<String>,
    deck_workers: [Sender<DeckWorkerEvent>; DECKS],
    app_status: AppStatus,
}

impl<const DECKS: usize> Tray<DECKS> {
    fn set_state(&mut self, state: TrayState) {
        self.state = state.clone();
        match self.published.write() {
            Ok(mut published) => *published = state,
            Err(_) => log::error!("Cannot publish tray state, lock poisoned (tui may be dead)"),
        }
    }

    fn handle_command(&mut self, command: RecordChangerCommand) {
        match command {
            RecordChangerCommand::Prepare { path } => self.prepare(path),
            RecordChangerCommand::Commit { deck_id } => self.commit(deck_id),
        }
    }

    /// Start decoding a track, unless one is already being decoded.
    fn prepare(&mut self, path: String) {
        if let TrayState::Decoding { path: current, .. } = &self.state {
            let msg = format!("Record loader busy, still decoding {current}");
            log::warn!("rejected {path}: {msg}");
            self.app_status.set(msg);
            return;
        }

        // Whatever was staged is replaced; freeing it here is fine, this thread
        // is not realtime.
        self.staged = None;
        self.app_status.set(format!("Loading: {path}"));
        log_try_send(&self.loader, path.clone(), "send track to loader");
        self.set_state(TrayState::Decoding {
            path,
            since: Instant::now(),
        });
    }

    /// Move the staged record onto a deck, emptying the tray.
    fn commit(&mut self, deck_id: DeckId) {
        let msg = match &self.state {
            TrayState::Decoding { path, .. } => {
                format!("Still decoding {path}, wait for it to be ready")
            }
            TrayState::Empty | TrayState::Failed { .. } => {
                "Nothing staged, drag and drop a music file first".to_string()
            }
            TrayState::Ready { .. } => {
                let Some(worker) = self.deck_workers.get(deck_id) else {
                    log::error!(
                        "cannot commit to deck {deck_id}, only {DECKS} decks are available"
                    );
                    return;
                };
                let Some((record, info)) = self.staged.take() else {
                    log::error!("tray is Ready but holds no record");
                    self.set_state(TrayState::Empty);
                    return;
                };
                log_try_send(
                    worker,
                    DeckWorkerEvent::ChangeRecord(record, info),
                    "hand staged record to deck",
                );
                self.set_state(TrayState::Empty);
                return;
            }
        };

        log::warn!("commit to deck {deck_id} rejected: {msg}");
        self.app_status.set(msg);
    }

    fn handle_outcome(&mut self, outcome: LoadOutcome) {
        let LoadOutcome { path, result } = outcome;
        match result {
            Ok((record, info)) => {
                self.app_status.set(format!(
                    "Ready: {path} - press Enter to load it on the active deck"
                ));
                self.staged = Some((record, info.clone()));
                self.set_state(TrayState::Ready { info });
            }
            Err(error) => {
                log::error!("failed to load track {path}: {error}");
                self.app_status
                    .set(format!("Failed to load {path}: {error}"));
                self.set_state(TrayState::Failed { path, error });
            }
        }
    }

    fn run(
        mut self,
        commands: Receiver<RecordChangerCommand>,
        outcomes: Receiver<LoadOutcome>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            select! {
                recv(commands) -> command => match command {
                    Ok(command) => self.handle_command(command),
                    Err(_) => break,
                },
                recv(outcomes) -> outcome => match outcome {
                    Ok(outcome) => self.handle_outcome(outcome),
                    Err(_) => break,
                },
                // so shutdown is noticed even when nothing is happening
                default(Duration::from_millis(100)) => (),
            }
        }
        log::debug!("Record tray terminated");
    }
}

/// Spawns the tray, the loader and the disposer, and returns a handle that
/// joins all three.
pub fn start<const DECKS: usize>(
    commands: Receiver<RecordChangerCommand>,
    deck_workers: [Sender<DeckWorkerEvent>; DECKS],
    tray_state: Arc<RwLock<TrayState>>,
    used_records: Receiver<Record>,
    app_status: AppStatus,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let (loader_tx, loader_rx) = crossbeam::channel::bounded::<String>(4);
    let (outcome_tx, outcome_rx) = crossbeam::channel::bounded::<LoadOutcome>(1);

    let loader = RecordLoader {
        paths: loader_rx,
        outcomes: outcome_tx,
        shutdown: Arc::clone(&shutdown),
    };

    let disposer = RecordDisposer {
        records: used_records,
        shutdown: Arc::clone(&shutdown),
    };

    let tray = Tray::<DECKS> {
        staged: None,
        state: TrayState::Empty,
        published: tray_state,
        loader: loader_tx,
        deck_workers,
        app_status,
    };

    let load_join = std::thread::spawn(move || loader.run());
    let drop_join = std::thread::spawn(move || disposer.run());
    let tray_join = {
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || tray.run(commands, outcome_rx, shutdown))
    };

    // Spawn supervisor thread to join all three workers cleanly
    std::thread::spawn(move || {
        tray_join.join().expect("record tray panicked");
        load_join.join().expect("record loader panicked");
        drop_join.join().expect("record disposer panicked");
        log::debug!("RecordChanger terminated cleanly");
    })
}
