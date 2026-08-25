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
    scratchv2::deck_controller::ExternalEvent,
    utils::log_try_send,
};

pub enum RecordChangerCommand {
    /// Load a track off disk
    Load(String),
    /// Hand an old record back to be deallocated off the audio thread
    Dispose(Record),
}

struct RecordDisposer {
    records: Receiver<Record>,
    shutdown: Arc<AtomicBool>,
}

struct RecordLoader {
    loading: Arc<AtomicBool>,
    records: Receiver<String>,
    controller: Sender<ExternalEvent>,
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

impl RecordLoader {
    fn run(self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            let track = match self.records.recv_timeout(Duration::from_millis(100)) {
                Ok(rec) => rec,
                Err(e) => match e {
                    crossbeam::channel::RecvTimeoutError::Timeout => continue,
                    crossbeam::channel::RecvTimeoutError::Disconnected => break,
                },
            };
            println!("Loading: {}", track);
            self.loading.store(true, Ordering::Relaxed);
            let rec = load_file(track.as_ref());

            match rec {
                Ok(rec) => {
                    let rec = Record::new(rec, Interpolator::linear());
                    log_try_send(
                        &self.controller,
                        ExternalEvent::ChangeRecord(rec),
                        "change record",
                    );
                    self.loading.store(false, Ordering::Relaxed);
                }
                Err(e) => {
                    log::error!("failed to load track {track}: {e}");
                    continue;
                }
            }
        }

        log::debug!("Record loader terminated");
    }
}

pub fn start(
    commands: Receiver<RecordChangerCommand>,
    controller: Sender<ExternalEvent>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let (loader_tx, loader_rx) = crossbeam::channel::bounded::<String>(4);
    let (disposer_tx, disposer_rx) = crossbeam::channel::bounded::<Record>(4);

    let loader_busy = Arc::new(AtomicBool::new(false));

    let loader = RecordLoader {
        records: loader_rx,
        controller,
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
                RecordChangerCommand::Load(path) => {
                    if !loader_busy.load(Ordering::Relaxed) {
                        log_try_send(&loader_tx, path, "load record");
                    } else {
                        println!("Record loader busy");
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
