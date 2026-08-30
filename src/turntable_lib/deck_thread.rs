//! Coordination module for the deck

use std::thread::JoinHandle;

use crossbeam::channel::Sender;

use crate::{
    deck_worker::{DeckWorker, DeckWorkerEvent},
    platter_driver::PlatterDriver,
};

pub struct DeckJoinHandle {
    platter_handle: JoinHandle<PlatterDriver>,
    worker_handle: JoinHandle<()>,
}

impl DeckJoinHandle {
    pub fn join(self) -> Option<PlatterDriver> {
        match self.worker_handle.join() {
            Ok(()) => (),
            Err(e) => log::error!("Deck worker panicked: {e:?}"),
        }

        match self.platter_handle.join() {
            Ok(platter_result) => Some(platter_result),
            Err(e) => {
                log::error!("Platter driver panicked: {e:?}");
                None
            }
        }
    }
}

pub type DeckId = usize;

pub struct DeckThread {
    deck_worker: DeckWorker,
    platter: PlatterDriver,
}

impl DeckThread {
    pub fn new(deck_worker: DeckWorker, platter: PlatterDriver) -> Self {
        Self {
            platter,
            deck_worker,
        }
    }

    /// Spawns deck worker and platter driver and returns the unified join handle
    pub fn start(self) -> DeckJoinHandle {
        let platter_handle = self.platter.start();
        let worker_handle = self.deck_worker.listen_to_external_events();

        DeckJoinHandle {
            platter_handle,
            worker_handle,
        }
    }

    pub fn deck_worker_channel(&self) -> Sender<DeckWorkerEvent> {
        self.deck_worker.get_event_sender()
    }
}
