use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use crossbeam::channel::{Receiver, Sender};
use rtrb::{Consumer, Producer, PushError};

use crate::{
    decoder::load_file,
    record::{Record, interpolation::Interpolator},
    scratchv2::deck_controller::ExternalEvent,
    utils::log_try_send,
};

// #[derive(Debug)]
pub struct RecordChanger {
    /// records to load and play next
    requested: Receiver<String>,
    /// channel to change playing record
    playing: Producer<Record>,
    /// used records that need to be dropped.
    /// (to make audio thread free of expensive dropping)
    used: Consumer<Record>,
    shutdown: Arc<AtomicBool>,
    /// to be able to stop record
    controller: Sender<ExternalEvent>,
}

impl RecordChanger {
    pub fn new(
        requested: Receiver<String>,
        playing: Producer<Record>,
        used: Consumer<Record>,
        shutdown: Arc<AtomicBool>,
        controller: Sender<ExternalEvent>,
    ) -> Self {
        Self {
            requested,
            playing,
            used,
            shutdown,
            controller,
        }
    }

    fn drop_records(mut used: Consumer<Record>, shutdown: Arc<AtomicBool>) {
        while !shutdown.load(Ordering::Relaxed) {
            match used.pop() {
                Ok(_record) => {
                    log::debug!("Record safely deallocated");
                    // Immediately check again without sleeping in case multiple records backed up
                    continue;
                }
                Err(rtrb::PopError::Empty) => {
                    // Sleep for 50ms to yield execution time back to the OS scheduler.
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        log::debug!("Record dropper terminated");
    }

    fn load_and_start_records(
        requested: Receiver<String>,
        mut playing: Producer<Record>,
        controller: Sender<ExternalEvent>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Relaxed) {
            let track = match requested.recv_timeout(Duration::from_millis(100)) {
                Ok(rec) => rec,
                Err(e) => match e {
                    crossbeam::channel::RecvTimeoutError::Timeout => continue,
                    crossbeam::channel::RecvTimeoutError::Disconnected => break,
                },
            };
            println!("Loading: {}", track);
            let rec = load_file(44100, track.as_ref());

            let rec = match rec {
                Ok(rec) => Record::new(rec, Interpolator::linear(), 44100),
                Err(e) => {
                    log::error!("failed to load track: {e}");
                    continue;
                }
            };

            log_try_send(&controller, ExternalEvent::RecordChanged, "reset playhead");
            match playing.push(rec) {
                Ok(()) => {}
                Err(PushError::Full(_)) => {
                    log::error!(
                        "failed to change the record as previous record change is still being done by the audio thread. Try again"
                    );
                }
            }
        }

        log::debug!("Record loader terminated");
    }

    pub fn start(self) -> JoinHandle<()> {
        let shutdown_copy = Arc::clone(&self.shutdown);
        let drop_join = std::thread::spawn(move || Self::drop_records(self.used, shutdown_copy));
        let load_join = std::thread::spawn(move || {
            Self::load_and_start_records(
                self.requested,
                self.playing,
                self.controller,
                self.shutdown,
            )
        });
        std::thread::spawn(move || {
            let _ = drop_join.join().expect("record dropper panicked");
            let _ = load_join.join().expect("record loader panicked");
        })
    }
}
