use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use crossbeam::channel::{Receiver, Sender};

use crate::{
    decoder::load_file,
    record::{Record, interpolation::Interpolator},
    scratchv2::{
        deck_controller::{DeckId, ExternalEvent},
        record_changer::LoaderResult::{NeedContinue, NeedTerminate},
    },
    utils::log_try_send,
};

pub enum RecordChangerCommand {
    /// Load a track off disk
    Load { deck_id: DeckId, path: String },
    /// Hand an old record back to be deallocated off the audio thread
    Dispose(Record),
}

struct RecordDisposer {
    records: Receiver<Record>,
    shutdown: Arc<AtomicBool>,
}

struct LoaderComm {
    records: Receiver<String>,
    controller: Sender<ExternalEvent>,
}

struct RecordLoader<const DECKS: usize> {
    loading: Arc<AtomicBool>,
    comm: [LoaderComm; DECKS],
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

enum LoaderResult {
    NeedTerminate,
    NeedContinue,
}

impl<const DECKS: usize> RecordLoader<DECKS> {
    fn try_load_record(loading: &AtomicBool, comm: &LoaderComm) -> LoaderResult {
        let track = match comm.records.recv_timeout(Duration::from_millis(100)) {
            Ok(rec) => rec,
            Err(e) => match e {
                crossbeam::channel::RecvTimeoutError::Timeout => return NeedContinue,
                crossbeam::channel::RecvTimeoutError::Disconnected => return NeedTerminate,
            },
        };
        println!("Loading: {}", track);
        loading.store(true, Ordering::Relaxed);
        let rec = load_file(track.as_ref());

        match rec {
            Ok(rec) => {
                let rec = Record::new(rec, Interpolator::linear());
                log_try_send(
                    &comm.controller,
                    ExternalEvent::ChangeRecord(rec),
                    "change record",
                );
                loading.store(false, Ordering::Relaxed);
            }
            Err(e) => {
                log::error!("failed to load track {track}: {e}");
            }
        };
        NeedContinue
    }

    fn run(self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            for comm in &self.comm {
                match Self::try_load_record(&self.loading, comm) {
                    NeedTerminate => break,
                    NeedContinue => (),
                }
            }
        }

        log::debug!("Record loader terminated");
    }
}

fn unzip_array<const N: usize, T1, T2>(a: [(T1, T2); N]) -> ([T1; N], [T2; N]) {
    let (v1, v2): (Vec<T1>, Vec<T2>) = a.into_iter().unzip();

    (
        v1.try_into().unwrap_or_else(|_| unreachable!()),
        v2.try_into().unwrap_or_else(|_| unreachable!()),
    )
}

pub fn start<const DECKS: usize>(
    commands: Receiver<RecordChangerCommand>,
    controllers: [Sender<ExternalEvent>; DECKS],
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let (disposer_tx, disposer_rx) = crossbeam::channel::bounded::<Record>(4);

    let loader_busy = Arc::new(AtomicBool::new(false));

    let (comm, loaders): ([LoaderComm; DECKS], [Sender<String>; DECKS]) =
        unzip_array(controllers.map(|controller| {
            let (loader_tx, loader_rx) = crossbeam::channel::bounded::<String>(4);
            (
                LoaderComm {
                    records: loader_rx,
                    controller,
                },
                loader_tx,
            )
        }));

    let loader = RecordLoader {
        comm,
        loading: Arc::clone(&loader_busy),
        shutdown: Arc::clone(&shutdown),
    };

    let disposer = RecordDisposer {
        records: disposer_rx,
        shutdown: Arc::clone(&shutdown),
    };

    let load_join = std::thread::spawn(move || loader.run());
    let drop_join = std::thread::spawn(move || disposer.run());

    let router_shutdown = Arc::clone(&shutdown);

    let router_join = std::thread::spawn(move || {
        while !router_shutdown.load(Ordering::Relaxed) {
            let cmd = match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => c,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            };

            match cmd {
                RecordChangerCommand::Load { deck_id, path } => {
                    if deck_id < DECKS {
                        if !loader_busy.load(Ordering::Relaxed) {
                            log_try_send(&loaders[deck_id], path, "load record");
                        } else {
                            println!("Record loader busy");
                        }
                    } else {
                        log::error!("invalid deck_id, only {DECKS} decks are available");
                    }
                }
                RecordChangerCommand::Dispose(record) => {
                    log_try_send(&disposer_tx, record, "dispose record");
                }
            }
        }
    });

    // Spawn supervisor thread to join all three workers cleanly
    std::thread::spawn(move || {
        router_join.join().expect("record changer panicked");
        load_join.join().expect("record loader panicked");
        drop_join.join().expect("record disposer panicked");
        log::debug!("RecordChanger terminated cleanly");
    })
}
