//! The record tray: holds one prepared record until it is loaded onto a deck.
//!
//! Loading a track is two steps, because that is what both input devices want:
//! scanning a card or dragging a file in only *prepares* the record, and a later
//! `LoadRecord` says which deck it goes on. In keyboard mode that second step is
//! a key press onto the active deck; a MIDI controller's LOAD buttons name the
//! deck themselves. Nothing here knows which of those is driving it.
//!
//! The tray holds what it prepared until something replaces it, so one record
//! reaches as many decks as it is asked to.
//!
//! This module owns the whole life of a [`Record`] outside the audio thread:
//! decoding it off disk, holding it, handing it to a deck, and freeing it once
//! the audio thread gives it back. Two threads:
//!
//! * **tray** — the state machine and the prepared record. Never blocks for
//!   long, so a `PrepareRecord` arriving mid-decode is queued right away instead
//!   of waiting out the decode it is queued behind.
//! * **loader** — blocking `load_file` calls, nothing else.

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
use rtrb::{Consumer, Producer};

use crate::{
    deck_controller::{DeckId, DeckState, RecordInfo},
    decoder::load_file,
    notices::Notices,
    platter_audio_processor::PlatterAudioProcessor,
    platter_driver::{Jump, PlatterEvent},
    record::{Record, interpolation::Interpolator},
};

pub enum TrayCommand {
    /// Decode a track off disk and hold it in the tray, ready to be loaded.
    PrepareRecord { path: String },
    /// Put the prepared record on a deck. The tray keeps it, so it can go on
    /// another deck too.
    LoadRecord { deck_id: DeckId },
}

/// What is in the tray right now.
///
/// Published for the TUI to render. The samples themselves never leave the tray
/// thread, so reading this never touches hundreds of megabytes.
#[derive(Debug, Clone)]
pub enum TrayState {
    /// Nothing prepared.
    Empty,
    /// Being decoded off disk. `queued` is the one waiting behind it, if a
    /// scan or a drop arrived while this was running.
    Preparing {
        path: String,
        since: Instant,
        queued: Option<String>,
    },
    /// Prepared. Stays here through any number of `LoadRecord`s, until another
    /// track is scanned or dropped.
    Ready { info: RecordInfo },
    /// The last record could not be prepared. The tray holds nothing.
    Failed { path: String, error: String },
}

/// Everything the tray needs to serve one deck.
///
/// Built by [`crate::deck_controller::new_deck`], which owns the other ends.
pub struct DeckSlot {
    /// hands a record to the audio thread
    pub records_in: Producer<Arc<Record>>,
    /// takes back the record the audio thread stopped using
    pub records_out: Consumer<Arc<Record>>,
    /// to publish what is playing on the deck
    pub state: Arc<DeckState>,
    /// to drop the needle at the start of a fresh record
    pub platter_events: Sender<PlatterEvent>,
}

/// One decoded record, handed from the loader back to the tray.
struct LoadedRecord {
    path: String,
    result: Result<(Arc<Record>, RecordInfo), String>,
}

/// The blocking decode, and nothing else.
///
/// Deliberately dumb, which is what keeps the tray responsive during a decode.
struct RecordLoader {
    paths: Receiver<String>,
    results: Sender<LoadedRecord>,
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
                        Arc::new(Record::new(samples, Interpolator::linear())),
                        RecordInfo {
                            path: path.clone(),
                            duration: PlatterAudioProcessor::frames_to_dur_nanos(samples_n),
                        },
                    ))
                }
                Err(e) => Err(e.to_string()),
            };

            if self.results.send(LoadedRecord { path, result }).is_err() {
                break;
            }
        }
        log::debug!("Record loader terminated");
    }
}

struct Tray<const DECKS: usize> {
    /// The prepared record, present exactly while `state` is
    /// [`TrayState::Ready`].
    prepared: Option<(Arc<Record>, RecordInfo)>,
    /// The track to decode once the current one finishes. Scans arrive without
    /// anyone pressing anything, so a request during a decode is held rather
    /// than lost; a newer one displaces it, since the DJ is holding the newer
    /// card.
    pending: Option<String>,
    /// Authoritative state; mirrored into `published` on every change.
    state: TrayState,
    published: Arc<RwLock<TrayState>>,
    loader: Sender<String>,
    decks: [DeckSlot; DECKS],
    notices: Notices,
}

impl<const DECKS: usize> Tray<DECKS> {
    fn set_state(&mut self, state: TrayState) {
        self.state = state.clone();
        match self.published.write() {
            Ok(mut published) => *published = state,
            Err(_) => log::error!("Cannot publish tray state, lock poisoned (tui may be dead)"),
        }
    }

    /// Frees the records the audio thread has finished with.
    ///
    /// Freeing a long record means unmapping hundreds of megabytes, which takes
    /// tens of milliseconds - fine on this thread, fatal on the audio thread,
    /// which is the whole reason the records come back through a queue.
    fn reclaim_records(&mut self) {
        for (deck_id, slot) in self.decks.iter_mut().enumerate() {
            while let Ok(used_record) = slot.records_out.pop() {
                drop(used_record);
                log::debug!("Record from deck {deck_id} freed");
            }
        }
    }

    fn handle_command(&mut self, command: TrayCommand) {
        match command {
            TrayCommand::PrepareRecord { path } => self.prepare_record(path),
            TrayCommand::LoadRecord { deck_id } => self.load_record(deck_id),
        }
    }

    /// Start decoding a track, or queue it if one is already being decoded.
    fn prepare_record(&mut self, path: String) {
        if let TrayState::Preparing {
            path: current,
            since,
            ..
        } = &self.state
        {
            log::info!("queued {path}, still preparing {current}");

            let waiting = TrayState::Preparing {
                path: current.clone(),
                since: *since,
                queued: Some(path.clone()),
            };
            self.pending = Some(path);
            self.set_state(waiting);
            return;
        }

        // whatever was prepared is replaced; freeing it here is fine
        self.prepared = None;

        // The loader's answer is the only thing that can move us out of
        // `Preparing`, so a request that never arrives would wedge the tray there
        // for good and reject every later record as busy. Never assume this one
        // was sent.
        if let Err(e) = self.loader.try_send(path.clone()) {
            let error = format!("record loader unreachable: {e}");
            log::error!("cannot prepare {path}: {error}");
            self.set_state(TrayState::Failed { path, error });
            return;
        }

        self.set_state(TrayState::Preparing {
            path,
            since: Instant::now(),
            queued: None,
        });
    }

    /// Put the prepared record on a deck, keeping it for the next one.
    fn load_record(&mut self, deck_id: DeckId) {
        let msg = match &self.state {
            TrayState::Preparing { path, .. } => {
                format!("Still preparing {path}, wait for it to be ready")
            }
            TrayState::Empty | TrayState::Failed { .. } => {
                "Nothing prepared, scan a card or drag and drop a music file first".to_string()
            }
            TrayState::Ready { .. } => {
                let Some(slot) = self.decks.get_mut(deck_id) else {
                    log::error!("cannot load on deck {deck_id}, only {DECKS} decks are available");
                    return;
                };
                // Cloned, not taken: the tray goes on holding the record, so the
                // same card can reach a second deck without being scanned again.
                // Sharing the samples is what makes that free.
                let Some((record, info)) = self.prepared.clone() else {
                    log::error!("tray is Ready but holds no record");
                    self.set_state(TrayState::Empty);
                    return;
                };

                match slot.records_in.push(record) {
                    Ok(()) => {
                        // The record is already on its way to the audio thread, so a
                        // lost reset cannot be retried here: it would start playing
                        // from wherever the previous record's playhead was. The deck's
                        // playhead is on screen, so the log is where this belongs.
                        match slot
                            .platter_events
                            .try_send(PlatterEvent::MovePlayhead(Jump::ToZero))
                        {
                            Ok(()) => {
                                log::info!("record {} loaded on deck {}", info.path, deck_id + 1)
                            }
                            Err(e) => log::error!(
                                "record {} loaded on deck {} but its playhead did not reset: {e}",
                                info.path,
                                deck_id + 1
                            ),
                        }

                        match slot.state.cur_record.write() {
                            Ok(mut cur_record) => *cur_record = Some(info),
                            Err(_) => log::error!(
                                "Cannot update current record info, lock poisoned (tui thread may be dead)"
                            ),
                        }
                    }
                    // The deck's hand-off slot holds one record and the audio
                    // callback empties it every block, so a full one means the
                    // audio thread has stopped running.
                    Err(rtrb::PushError::Full(_)) => {
                        log::error!("deck {deck_id} never took the last record");
                        self.notices.error(format!(
                            "Deck {} has not taken the last record - is audio running?",
                            deck_id + 1
                        ));
                    }
                }
                return;
            }
        };

        log::warn!("load on deck {deck_id} rejected: {msg}");
        self.notices.warn(msg);
    }

    fn handle_loaded_record(&mut self, loaded: LoadedRecord) {
        let LoadedRecord { path, result } = loaded;
        match result {
            Ok((record, info)) => {
                log::info!("ready: {path}");
                self.prepared = Some((record, info.clone()));
                self.set_state(TrayState::Ready { info });
            }
            Err(error) => {
                log::error!("failed to prepare track {path}: {error}");
                self.set_state(TrayState::Failed { path, error });
            }
        }

        // The loader is free again, so whatever arrived while it was busy runs
        // now. Taken after the state is published, so `prepare_record` sees a
        // tray that is no longer `Preparing`.
        if let Some(next) = self.pending.take() {
            self.prepare_record(next);
        }
    }

    fn run(
        mut self,
        commands: Receiver<TrayCommand>,
        loaded: Receiver<LoadedRecord>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            self.reclaim_records();

            select! {
                recv(commands) -> command => match command {
                    Ok(command) => self.handle_command(command),
                    Err(_) => break,
                },
                recv(loaded) -> loaded => match loaded {
                    Ok(loaded) => self.handle_loaded_record(loaded),
                    Err(_) => break,
                },
                // so shutdown and returned records are noticed when nothing happens
                default(Duration::from_millis(100)) => (),
            }
        }
        log::debug!("Record tray terminated");
    }
}

/// Spawns the tray and the record loader, and returns a handle that joins both.
pub fn start<const DECKS: usize>(
    commands: Receiver<TrayCommand>,
    decks: [DeckSlot; DECKS],
    tray_state: Arc<RwLock<TrayState>>,
    notices: Notices,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    // one decode at a time: the tray only ever sends a path when it is not
    // already `Preparing`, so a full channel means that invariant broke
    let (path_tx, path_rx) = crossbeam::channel::bounded::<String>(1);
    let (loaded_tx, loaded_rx) = crossbeam::channel::bounded::<LoadedRecord>(1);

    let loader = RecordLoader {
        paths: path_rx,
        results: loaded_tx,
        shutdown: Arc::clone(&shutdown),
    };

    let tray = Tray::<DECKS> {
        prepared: None,
        pending: None,
        state: TrayState::Empty,
        published: tray_state,
        loader: path_tx,
        decks,
        notices,
    };

    let loader_join = std::thread::spawn(move || loader.run());
    let tray_join = {
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || tray.run(commands, loaded_rx, shutdown))
    };

    // Spawn supervisor thread to join both workers cleanly
    std::thread::spawn(move || {
        tray_join.join().expect("record tray panicked");
        loader_join.join().expect("record loader panicked");
        log::debug!("Record tray terminated cleanly");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam::channel::bounded;

    /// A tray with no decks, which is all the queueing needs. The receiver is
    /// what the record loader would be reading.
    fn tray_and_loader() -> (Tray<0>, Receiver<String>) {
        let (loader, decoding) = bounded(1);

        let tray = Tray::<0> {
            prepared: None,
            pending: None,
            state: TrayState::Empty,
            published: Arc::new(RwLock::new(TrayState::Empty)),
            loader,
            decks: [],
            notices: Notices::new(),
        };

        (tray, decoding)
    }

    /// Whatever the loader was handed, if anything.
    fn started(decoding: &Receiver<String>) -> Option<String> {
        decoding.try_recv().ok()
    }

    /// Standing in for a decode that finished, without building a record.
    fn finished(path: &str) -> LoadedRecord {
        LoadedRecord {
            path: path.to_string(),
            result: Err("not a real decode".to_string()),
        }
    }

    /// Scans arrive without anyone pressing anything, so a second one during a
    /// decode has to wait rather than be dropped.
    #[test]
    fn a_request_during_a_decode_runs_after_it() {
        let (mut tray, decoding) = tray_and_loader();

        tray.prepare_record("first.flac".to_string());
        assert_eq!(started(&decoding).as_deref(), Some("first.flac"));

        tray.prepare_record("second.flac".to_string());
        assert_eq!(started(&decoding), None, "two decodes were started at once");

        tray.handle_loaded_record(finished("first.flac"));
        assert_eq!(started(&decoding).as_deref(), Some("second.flac"));
    }

    /// The DJ is holding the newest card, so that is the one to end up with.
    #[test]
    fn the_newest_waiting_request_displaces_the_others() {
        let (mut tray, decoding) = tray_and_loader();

        tray.prepare_record("first.flac".to_string());
        let _ = started(&decoding);

        tray.prepare_record("second.flac".to_string());
        tray.prepare_record("third.flac".to_string());

        tray.handle_loaded_record(finished("first.flac"));
        assert_eq!(started(&decoding).as_deref(), Some("third.flac"));

        tray.handle_loaded_record(finished("third.flac"));
        assert_eq!(started(&decoding), None, "a displaced request still ran");
    }

    #[test]
    fn nothing_waits_when_the_loader_was_idle() {
        let (mut tray, decoding) = tray_and_loader();

        tray.prepare_record("only.flac".to_string());
        assert_eq!(started(&decoding).as_deref(), Some("only.flac"));

        tray.handle_loaded_record(finished("only.flac"));
        assert_eq!(started(&decoding), None);
    }
}
