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
    deck_thread::DeckId,
    platter_driver::{Jump, PlatterEvent},
    record::Record,
    record_changer::RecordChangerCommand,
    utils::log_try_send,
};

#[derive(Debug)]
pub enum DeckWorkerEvent {
    /// Load the record
    LoadRecord(String),
    /// Change record after successful loading
    ChangeRecord(Record),
}

/// holds deck background thread that handles external events
pub struct DeckWorker {
    deck_id: DeckId,
    shutdown: Arc<AtomicBool>,
    adjust_playhead: Sender<PlatterEvent>,
    // Receives events from outside senders
    event_receiver: Receiver<DeckWorkerEvent>,
    // so the controller can clone and hand out senders
    event_sender: Sender<DeckWorkerEvent>,
    /// communication channel with record changer
    record_changer: Sender<RecordChangerCommand>,

    /// audio thread sends used records
    dispose_record: Consumer<Record>,
    /// send new record to audio thread
    change_record: Producer<Record>,
}

impl DeckWorker {
    pub fn new(
        deck_id: DeckId,
        adjust_playhead: Sender<PlatterEvent>,
        event_receiver: Receiver<DeckWorkerEvent>,
        event_sender: Sender<DeckWorkerEvent>,
        record_changer: Sender<RecordChangerCommand>,
        dispose_record: Consumer<Record>,
        change_record: Producer<Record>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            deck_id,
            shutdown,
            adjust_playhead,
            event_receiver,
            event_sender,
            record_changer,
            dispose_record,
            change_record,
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
                        &self.record_changer,
                        RecordChangerCommand::Dispose(used_record),
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
            DeckWorkerEvent::ChangeRecord(record) => match self.change_record.push(record) {
                Ok(()) => {
                    println!("Record loaded to deck {}", self.deck_id);
                    log_try_send(
                        &self.adjust_playhead,
                        PlatterEvent::MovePlayhead(Jump::ToZero),
                        "reset playhead",
                    )
                }
                Err(rtrb::PushError::Full(rejected_record)) => {
                    log::error!("Failed to change record, audio thread record queue is full");
                    log_try_send(
                        &self.record_changer,
                        RecordChangerCommand::Dispose(rejected_record),
                        "dispose rejected record",
                    );
                }
            },
            DeckWorkerEvent::LoadRecord(record) => {
                log_try_send(
                    &self.record_changer,
                    RecordChangerCommand::Load {
                        deck_id: self.deck_id,
                        path: record,
                    },
                    "request load track",
                );
            }
        }
    }
}
