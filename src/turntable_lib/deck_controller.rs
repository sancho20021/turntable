use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use atomic_float::AtomicF64;
use crossbeam::{
    atomic::AtomicCell,
    channel::{Sender, bounded},
};

use crate::{
    audio_health::DeckHealth,
    filters::FirstOrderLPF,
    input_event::{DeckCommand, DeckEvent},
    input_profile::InputProfile,
    platter_audio_processor::AudioProcessorHandles,
    platter_driver::{Jump, PlatterDriver, PlatterEvent},
    record::{INanos, UNanos},
    record_input,
    telemetry::TelemetryTrace,
    tray::{DeckSlot, TrayCommand},
    utils::log_try_send,
    virtual_platter::{ReadablePlatter, new_platter},
};

/// Which deck a command is addressed to. `0` is deck 1.
pub type DeckId = usize;

// const SPEED_EPS: f64 = 0.001;

#[derive(Debug, Clone, Copy)]
pub enum PlatterState {
    Playing,
    Scratching {
        /// The exact state of the virtual platter when the platter was grabbed
        anchor_pos: INanos,
        /// The input position (in input units) when the platter was grabbed
        anchor_input: i64,
        /// The latest input position reported by the input device
        latest_input: i64,
        /// Latest input speed in input units / sec, if known
        input_speed: Option<f64>,
        timestamp: UNanos,
    },
}

#[derive(Debug, Clone)]
pub struct RecordInfo {
    pub path: String,
    pub duration: UNanos,
}

#[derive(Debug)]
pub struct DeckState {
    /// Target platter speed (1.0 = normal)
    pub pitch: AtomicF64,
    /// Is deck playing
    pub playing: AtomicBool,
    /// Platter state (scratching or playing)
    pub platter: AtomicCell<PlatterState>,
    /// Current record playing
    pub cur_record: RwLock<Option<RecordInfo>>,
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
    deck_id: DeckId,
    state: Arc<DeckState>,
    /// to prepare records and load them onto this deck, see [`crate::tray`]
    tray: Sender<TrayCommand>,
    platter_events: Sender<PlatterEvent>,
    platter: ReadablePlatter,
    /// input speed smoothing, see [`InputProfile::speed_smoothing_tau_secs`]
    input_speed: FirstOrderLPF,
    /// For recording metrics
    pub tracer: TelemetryTrace,
}

#[derive(Debug, Clone, Copy)]
enum PitchUpdate {
    Reset,
    Set(f64),
    Adjust(f64),
}

pub fn new_deck(
    deck_id: DeckId,
    input: InputProfile,
    inertia_tau_secs: f64,
    tray: Sender<TrayCommand>,
    shutdown: Arc<AtomicBool>,
    buffer_frames_n: usize,
    health: Arc<DeckHealth>,
) -> (
    DeckController,
    PlatterDriver,
    AudioProcessorHandles,
    DeckSlot,
) {
    let initial_state = Arc::new(DeckState {
        pitch: AtomicF64::new(1.0),
        playing: AtomicBool::new(true),
        platter: AtomicCell::new(PlatterState::Playing),
        cur_record: RwLock::new(None),
    });
    let (pl_snd, pl_rcv) = bounded(1000);
    let (writable_platter, readable_platter) = new_platter();

    let (used_records_prod, used_records_cons) = rtrb::RingBuffer::new(3);
    let (new_record_prod, new_record_cons) = rtrb::RingBuffer::new(1);
    let audio_handles = AudioProcessorHandles {
        next_record: new_record_cons,
        used_records: used_records_prod,
        platter: readable_platter.clone(),
        health,
    };
    let deck_slot = DeckSlot {
        records_in: new_record_prod,
        records_out: used_records_cons,
        state: Arc::clone(&initial_state),
        platter_events: pl_snd.clone(),
    };

    let driver = PlatterDriver::new(
        deck_id,
        Arc::clone(&initial_state),
        input.clone(),
        inertia_tau_secs,
        writable_platter,
        pl_rcv,
        shutdown,
        buffer_frames_n,
    );

    (
        DeckController {
            deck_id,
            state: Arc::clone(&initial_state),
            tray,
            platter: readable_platter,
            platter_events: pl_snd,
            tracer: TelemetryTrace::new(),
            input_speed: FirstOrderLPF::new(input.speed_smoothing_tau_secs),
        },
        driver,
        audio_handles,
        deck_slot,
    )
}

impl DeckController {
    fn handle_scratch_move(&mut self, x: i64, current: PlatterState, when: Instant) {
        if let PlatterState::Scratching {
            latest_input,
            anchor_pos,
            anchor_input,
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
                        self.input_speed
                            .advance(dt_secs, (x - latest_input) as f64 / dt_secs),
                    )
                }
            };
            let new_state = PlatterState::Scratching {
                anchor_pos,
                anchor_input,
                latest_input: x,
                input_speed: speed,
                timestamp: new_timestamp,
            };
            self.state.platter.store(new_state);
            record_input!(
                self.tracer,
                new_timestamp,
                format!("latest_input_{}", self.deck_id),
                x as f64
            );
            if let Some(s) = speed {
                record_input!(
                    self.tracer,
                    new_timestamp,
                    format!("raw_input_speed_{}", self.deck_id),
                    s
                );
                record_input!(
                    self.tracer,
                    new_timestamp,
                    format!("input_dt_us_{}", self.deck_id),
                    dt_secs * 1_000_000.
                );
            }
        }
    }

    fn handle_scratch_start(&mut self, x: i64, current: PlatterState, when: Instant) {
        if let PlatterState::Playing = current {
            let tstmp = self.platter.timestamp(when);
            let scratch_state = PlatterState::Scratching {
                anchor_pos: self.platter.get_playhead().record_pos,
                anchor_input: x,
                latest_input: x,
                input_speed: None,
                timestamp: tstmp,
            };
            self.state.platter.store(scratch_state);
            record_input!(
                self.tracer,
                tstmp,
                format!("latest_input_{}", self.deck_id),
                x as f64
            )
        }
    }

    fn handle_scratch_end(&mut self) {
        self.state.platter.store(PlatterState::Playing);
        self.input_speed.reset();
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

    pub fn handle_deck_event(&mut self, event: DeckEvent) {
        let current_pitch = self.state.pitch.load(Ordering::Relaxed);
        let current_state = self.state.platter.load();
        match event.command {
            DeckCommand::ScratchMove(pos) => {
                self.handle_scratch_move(pos, current_state, event.timestamp)
            }
            DeckCommand::ScratchStart(pos) => {
                self.handle_scratch_start(pos, current_state, event.timestamp)
            }
            DeckCommand::ScratchEnd => self.handle_scratch_end(),
            DeckCommand::ResetPitch => self.update_pitch(PitchUpdate::Reset, current_pitch),
            DeckCommand::SetPitch(pitch) => {
                self.update_pitch(PitchUpdate::Set(pitch), current_pitch)
            }
            DeckCommand::PitchUp => self.update_pitch(PitchUpdate::Adjust(0.01), current_pitch),
            DeckCommand::PitchDown => self.update_pitch(PitchUpdate::Adjust(-0.01), current_pitch),
            DeckCommand::StartStop => self.start_or_stop(),
            DeckCommand::LoadRecord => log_try_send(
                &self.tray,
                TrayCommand::LoadRecord {
                    deck_id: self.deck_id,
                },
                "load record from tray",
            ),
            DeckCommand::PlayheadReset => log_try_send(
                &self.platter_events,
                PlatterEvent::MovePlayhead(Jump::ToZero),
                "reset playhead",
            ),
            DeckCommand::PlayheadFF => log_try_send(
                &self.platter_events,
                PlatterEvent::MovePlayhead(Jump::Forward),
                "fast forward",
            ),
            DeckCommand::PlayheadRewind => log_try_send(
                &self.platter_events,
                PlatterEvent::MovePlayhead(Jump::Backward),
                "rewind",
            ),
            DeckCommand::Nudge(x) => {
                log_try_send(&self.platter_events, PlatterEvent::Nudge(x), "nudge");
            }
        };
    }

    pub fn get_state(&self) -> Arc<DeckState> {
        Arc::clone(&self.state)
    }

    pub fn get_platter(&self) -> ReadablePlatter {
        self.platter.clone()
    }
}

#[derive(Clone)]
pub struct AppStatus {
    pub message: Arc<RwLock<String>>,
}

impl AppStatus {
    pub fn new() -> Self {
        Self {
            message: Arc::new(RwLock::new(
                "Drag and drop a music file to start".to_string(),
            )),
        }
    }

    pub fn set(&self, msg: impl Into<String>) {
        if let Ok(mut lock) = self.message.write() {
            *lock = msg.into();
        }
    }

    pub fn get(&self) -> Option<String> {
        self.message.read().ok().map(|m| m.clone())
    }
}
