use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use atomic_float::AtomicF64;
use crossbeam::{
    atomic::AtomicCell,
    channel::{Receiver, Sender, bounded},
};
use rtrb::{Consumer, Producer};

use crate::{
    deck_event::{self, DeckEvent},
    filters::FirstOrderLPF,
    record::{INanos, Record, UNanos},
    record_mouse,
    scratchv2::{
        platter_driver::{Jump, PlatterDriver, PlatterEvent},
        record_changer::RecordChangerCommand,
        virtual_platter::{ReadablePlatter, WritablePlatter},
    },
    telemetry::TelemetryTrace,
    utils::log_try_send,
};

// const SPEED_EPS: f64 = 0.001;

#[derive(Debug, Clone, Copy)]
pub enum PlatterState {
    Playing,
    Scratching {
        /// The exact state of the virtual platter when the mouse went down
        anchor_pos: INanos,
        /// The mouse X position when the mouse went down
        anchor_mouse_x: i32,
        /// The latest mouse position sent by the OS
        latest_mouse_x: i32,
        /// latest mouse speed in i32/sec, if known
        mouse_speed: Option<f64>,
        timestamp: UNanos,
    },
}

#[derive(Debug)]
pub struct DeckState {
    /// Target platter speed (1.0 = normal)
    pub pitch: AtomicF64,
    /// Is deck playing
    pub playing: AtomicBool,
    /// Platter state (scratching or playing)
    pub platter: AtomicCell<PlatterState>,
}

impl DeckState {
    /// target platter speed.
    ///
    /// 0 if not playing, pitch if playing
    pub fn target_speed(&self) -> f64 {
        if self.playing.load(Ordering::Relaxed) {
            self.pitch.load(Ordering::Relaxed)
        } else {
            0.
        }
    }
}

/// Deck controller that can be used to update virtual platter
/// based on the state which can be either normal playback or scratching mode.
pub struct DeckController {
    state: Arc<DeckState>,
    deck_worker: Sender<ExternalEvent>,
    platter_events: Sender<PlatterEvent>,
    platter: ReadablePlatter,
    /// mouse speed smoothing
    mouse_speed: FirstOrderLPF,
    /// For recording metrics
    pub tracer: TelemetryTrace,
}

#[derive(Debug, Clone, Copy)]
enum PitchUpdate {
    Reset,
    Set(f64),
    Adjust(f64),
}

#[derive(Debug)]
pub enum ExternalEvent {
    /// Load the record
    LoadRecord(String),
    /// Change record after successful loading
    ChangeRecord(Record),
}

/// holds deck background thread that handles external events
pub struct DeckWorker {
    adjust_playhead: Sender<PlatterEvent>,
    // Receives events from outside senders
    event_receiver: Receiver<ExternalEvent>,
    // so the controller can clone and hand out senders
    event_sender: Sender<ExternalEvent>,
    /// communication channel with record changer
    record_changer: Sender<RecordChangerCommand>,

    /// audio thread sends used records
    dispose_record: Consumer<Record>,
    /// send new record to audio thread
    change_record: Producer<Record>,
}

static BASE_SENSITIVITY_FACTOR: f64 = 1_500_000.0;

impl DeckController {
    pub fn new(
        readable_platter: ReadablePlatter,
        writable_platter: WritablePlatter,
        initial_pitch: f64,
        sensitivity: f64,
        inertia_tau_secs: f64,
        nudge_responsiveness: f32,
        record_changer: Sender<RecordChangerCommand>,
    ) -> (
        Self,
        PlatterDriver,
        DeckWorker,
        Producer<Record>,
        Consumer<Record>,
    ) {
        let initial_state = Arc::new(DeckState {
            pitch: AtomicF64::new(initial_pitch),
            playing: AtomicBool::new(true),
            platter: AtomicCell::new(PlatterState::Playing),
        });
        let (pl_snd, pl_rcv) = bounded(1000);

        let driver = PlatterDriver::new(
            Arc::clone(&initial_state),
            sensitivity * BASE_SENSITIVITY_FACTOR,
            inertia_tau_secs,
            writable_platter,
            pl_rcv,
            nudge_responsiveness,
        );
        let (event_snd, event_rcv) = bounded(100);

        let (used_records_prod, used_records_cons) = rtrb::RingBuffer::new(3);
        let (new_record_prod, new_record_cons) = rtrb::RingBuffer::new(1);
        let deck_worker = DeckWorker {
            adjust_playhead: pl_snd.clone(),
            event_receiver: event_rcv,
            event_sender: event_snd,
            record_changer,
            dispose_record: used_records_cons,
            change_record: new_record_prod,
        };

        (
            Self {
                state: Arc::clone(&initial_state),
                platter: readable_platter,
                deck_worker: deck_worker.get_event_sender(),
                platter_events: pl_snd,
                tracer: TelemetryTrace::new(),
                mouse_speed: FirstOrderLPF::new(0.01),
            },
            driver,
            deck_worker,
            used_records_prod,
            new_record_cons,
        )
    }

    fn handle_mouse_motion(&mut self, x: i32, current: PlatterState, when: Instant) {
        if let PlatterState::Scratching {
            latest_mouse_x,
            anchor_pos,
            anchor_mouse_x,
            timestamp: prev_timestamp,
            ..
        } = current
        {
            let new_timestamp = self.platter.timestamp(when);
            let dt_secs: f64 = (new_timestamp.0 as f64 - prev_timestamp.0 as f64) / 1_000_000_000.;
            let speed = {
                if dt_secs <= 0. {
                    None
                } else {
                    Some(
                        self.mouse_speed
                            .advance(dt_secs, (x - latest_mouse_x) as f64 / dt_secs),
                    )
                }
            };
            let new_state = PlatterState::Scratching {
                anchor_pos,
                anchor_mouse_x,
                latest_mouse_x: x,
                mouse_speed: speed,
                timestamp: new_timestamp,
            };
            self.state.platter.store(new_state);
            record_mouse!(self.tracer, new_timestamp, "latest_mouse_x", x as f64);
            if let Some(s) = speed {
                record_mouse!(self.tracer, new_timestamp, "raw_mouse_speed", s);
                record_mouse!(
                    self.tracer,
                    new_timestamp,
                    "mouse_dt_us",
                    dt_secs * 1_000_000.
                );
            }
        }
    }

    fn handle_mouse_down(&mut self, x: i32, current: PlatterState, when: Instant) {
        if let PlatterState::Playing = current {
            let tstmp = self.platter.timestamp(when);
            let scratch_state = PlatterState::Scratching {
                anchor_pos: self.platter.get_playhead().record_pos,
                anchor_mouse_x: x,
                latest_mouse_x: x,
                mouse_speed: None,
                timestamp: tstmp,
            };
            self.state.platter.store(scratch_state);
            record_mouse!(self.tracer, tstmp, "latest_mouse_x", x as f64)
        }
    }

    fn handle_mouse_up(&mut self) {
        self.state.platter.store(PlatterState::Playing);
        self.mouse_speed.reset();
    }

    fn update_pitch(&self, update: PitchUpdate, cur_speed: f64) {
        let new_speed = match update {
            PitchUpdate::Reset => 1.,
            PitchUpdate::Set(x) => x,
            PitchUpdate::Adjust(delta) => cur_speed + delta,
        };
        self.state.pitch.store(new_speed, Ordering::Relaxed);
    }

    fn start_or_stop(&self) {
        let playing = self.state.playing.load(Ordering::Relaxed);
        self.state.playing.store(!playing, Ordering::Relaxed);
    }

    pub fn handle_deck_event(&mut self, event: DeckEvent) -> anyhow::Result<()> {
        let current_pitch = self.state.pitch.load(Ordering::Relaxed);
        let current_state = self.state.platter.load();
        match event.event {
            deck_event::Event::MouseMotion(pos) => {
                self.handle_mouse_motion(pos, current_state, event.timestamp)
            }
            deck_event::Event::MouseDown(pos) => {
                self.handle_mouse_down(pos, current_state, event.timestamp)
            }
            deck_event::Event::MouseUp(_) => self.handle_mouse_up(),
            deck_event::Event::ResetPitch => self.update_pitch(PitchUpdate::Reset, current_pitch),
            deck_event::Event::PitchUp => {
                self.update_pitch(PitchUpdate::Adjust(0.01), current_pitch)
            }
            deck_event::Event::PitchDown => {
                self.update_pitch(PitchUpdate::Adjust(-0.01), current_pitch)
            }
            deck_event::Event::StartStop => self.start_or_stop(),
            deck_event::Event::LoadTrack(track) => log_try_send(
                &self.deck_worker,
                ExternalEvent::LoadRecord(track),
                "load record",
            ),
            deck_event::Event::PlayheadReset => log_try_send(
                &self.platter_events,
                PlatterEvent::MovePlayhead(Jump::ToZero),
                "reset playhead",
            ),
            deck_event::Event::PlayheadFF => log_try_send(
                &self.platter_events,
                PlatterEvent::MovePlayhead(Jump::Forward),
                "fast forward",
            ),
            deck_event::Event::PlayheadRewind => log_try_send(
                &self.platter_events,
                PlatterEvent::MovePlayhead(Jump::Backward),
                "rewind",
            ),
            deck_event::Event::Nudge(x) => {
                log_try_send(&self.platter_events, PlatterEvent::Nudge(x), "nudge");
            }
        };
        Ok(())
    }
}

impl DeckWorker {
    pub fn get_event_sender(&self) -> Sender<ExternalEvent> {
        self.event_sender.clone()
    }

    /// Starts a background thread that connects external components (like the **`RecordChanger`**) to this controller.
    ///
    /// **Must be called at startup**
    pub fn listen_to_external_events(mut self, shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
        std::thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
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

    fn process_external_event(&mut self, event: ExternalEvent) {
        match event {
            ExternalEvent::ChangeRecord(record) => match self.change_record.push(record) {
                Ok(()) => {
                    println!("Record loaded");
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
            ExternalEvent::LoadRecord(record) => {
                log_try_send(
                    &self.record_changer,
                    RecordChangerCommand::Load(record),
                    "request load track",
                );
            }
        }
    }
}
