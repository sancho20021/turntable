use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use crossbeam::channel::{Receiver, Sender};
use rtrb::{Consumer, Producer};

use crate::{
    deck_controller::{AppStatus, DeckState, RecordInfo},
    deck_thread::DeckId,
    platter_driver::{Jump, PlatterEvent},
    record::Record,
    utils::log_try_send,
};

#[derive(Debug)]
pub enum DeckWorkerEvent {
    /// Put a record staged by the tray on this deck
    ChangeRecord(Record, RecordInfo),
}

/// holds deck background thread that handles external events
pub struct DeckWorker {
    deck_id: DeckId,
    shutdown: Arc<AtomicBool>,
    // to update deck state
    deck_state: Arc<DeckState>,
    adjust_playhead: Sender<PlatterEvent>,
    // Receives events from outside senders
    event_receiver: Receiver<DeckWorkerEvent>,
    // so the controller can clone and hand out senders
    event_sender: Sender<DeckWorkerEvent>,
    /// records handed back by the audio thread go here to be freed
    disposer: Sender<Record>,

    /// audio thread sends used records
    dispose_record: Consumer<Record>,
    /// send new record to audio thread
    change_record: Producer<Record>,
    app_status: AppStatus,
}

impl DeckWorker {
    pub fn new(
        deck_id: DeckId,
        adjust_playhead: Sender<PlatterEvent>,
        event_receiver: Receiver<DeckWorkerEvent>,
        event_sender: Sender<DeckWorkerEvent>,
        disposer: Sender<Record>,
        dispose_record: Consumer<Record>,
        change_record: Producer<Record>,
        shutdown: Arc<AtomicBool>,
        deck_state: Arc<DeckState>,
        app_status: AppStatus,
    ) -> Self {
        Self {
            deck_id,
            shutdown,
            deck_state,
            adjust_playhead,
            event_receiver,
            event_sender,
            disposer,
            dispose_record,
            change_record,
            app_status,
        }
    }
    pub fn get_event_sender(&self) -> Sender<DeckWorkerEvent> {
        self.event_sender.clone()
    }

    /// Starts a background thread that connects external components (like the **`RecordChanger`**) to this controller.
    ///
    /// **Must be called at startup**
    pub fn listen_to_external_events(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            while !self.shutdown.load(Ordering::Relaxed) {
                // 1. Drain used records returned from the audio thread
                while let Ok(used_record) = self.dispose_record.pop() {
                    log_try_send(
                        &self.disposer,
                        used_record,
                        "forward returned record to disposer",
                    );
                }

                let event = match self.event_receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => event,
                    Err(e) => match e {
                        crossbeam::channel::RecvTimeoutError::Timeout => continue,
                        crossbeam::channel::RecvTimeoutError::Disconnected => break,
                    },
                };
                self.process_external_event(event);
            }
        })
    }

    fn process_external_event(&mut self, event: DeckWorkerEvent) {
        match event {
            DeckWorkerEvent::ChangeRecord(record, record_info) => {
                match self.change_record.push(record) {
                    Ok(()) => {
                        {
                            let msg = format!(
                                "Record {} loaded to deck {}",
                                record_info.path, self.deck_id
                            );
                            log::info!("{msg}");
                            self.app_status.set(msg);
                        }

                        log_try_send(
                            &self.adjust_playhead,
                            PlatterEvent::MovePlayhead(Jump::ToZero),
                            "reset playhead",
                        );
                        let mut cur_rec = match self.deck_state.cur_record.write() {
                            Ok(cur_rec) => cur_rec,
                            Err(_) => {
                                log::error!(
                                    "Cannot update current record info, lock poisoned (tui thread may be dead)"
                                );
                                return;
                            }
                        };
                        *cur_rec = Some(record_info);
                    }
                    Err(rtrb::PushError::Full(rejected_record)) => {
                        log::error!("Failed to change record, audio thread record queue is full");
                        log_try_send(&self.disposer, rejected_record, "dispose rejected record");
                    }
                }
            }
        }
    }
}
