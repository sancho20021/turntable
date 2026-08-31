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
    deck_event::{self, DeckEvent},
    deck_thread::{DeckId, DeckThread},
    deck_worker::{DeckWorker, DeckWorkerEvent},
    filters::FirstOrderLPF,
    input_profile::InputProfile,
    platter_audio_processor::AudioProcessorHandles,
    platter_driver::{Jump, PlatterDriver, PlatterEvent},
    record::{INanos, UNanos},
    record_changer::RecordChangerCommand,
    record_mouse,
    telemetry::TelemetryTrace,
    utils::log_try_send,
    virtual_platter::{ReadablePlatter, new_platter},
};

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
    state: Arc<DeckState>,
    deck_worker: Sender<DeckWorkerEvent>,
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
    initial_pitch: f64,
    input: InputProfile,
    inertia_tau_secs: f64,
    nudge_responsiveness: f32,
    record_changer: Sender<RecordChangerCommand>,
    shutdown: Arc<AtomicBool>,
    buffer_frames_n: usize,
    app_status: &AppStatus,
) -> (DeckController, DeckThread, AudioProcessorHandles) {
    let initial_state = Arc::new(DeckState {
        pitch: AtomicF64::new(initial_pitch),
        playing: AtomicBool::new(true),
        platter: AtomicCell::new(PlatterState::Playing),
        cur_record: RwLock::new(None),
    });
    let (pl_snd, pl_rcv) = bounded(1000);
    let (writable_platter, readable_platter) = new_platter();

    let (event_snd, event_rcv) = bounded(100);

    let (used_records_prod, used_records_cons) = rtrb::RingBuffer::new(3);
    let (new_record_prod, new_record_cons) = rtrb::RingBuffer::new(1);
    let audio_handles = AudioProcessorHandles {
        next_record: new_record_cons,
        used_records: used_records_prod,
        platter: readable_platter.clone(),
    };

    let deck_worker = DeckWorker::new(
        deck_id,
        pl_snd.clone(),
        event_rcv,
        event_snd,
        record_changer,
        used_records_cons,
        new_record_prod,
        Arc::clone(&shutdown),
        Arc::clone(&initial_state),
        app_status.clone(),
    );
    let driver = PlatterDriver::new(
        deck_id,
        Arc::clone(&initial_state),
        input,
        inertia_tau_secs,
        writable_platter,
        pl_rcv,
        nudge_responsiveness,
        shutdown,
        buffer_frames_n,
    );

    let deck_thread = DeckThread::new(deck_worker, driver);
    (
        DeckController {
            state: Arc::clone(&initial_state),
            platter: readable_platter,
            deck_worker: deck_thread.deck_worker_channel(),
            platter_events: pl_snd,
            tracer: TelemetryTrace::new(),
            input_speed: FirstOrderLPF::new(input.speed_smoothing_tau_secs),
        },
        deck_thread,
        audio_handles,
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
            record_mouse!(self.tracer, tstmp, "latest_mouse_x", x as f64)
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

    pub fn handle_deck_event(&mut self, event: DeckEvent) -> anyhow::Result<()> {
        let current_pitch = self.state.pitch.load(Ordering::Relaxed);
        let current_state = self.state.platter.load();
        match event.event {
            deck_event::Event::ScratchMove(pos) => {
                self.handle_scratch_move(pos, current_state, event.timestamp)
            }
            deck_event::Event::ScratchStart(pos) => {
                self.handle_scratch_start(pos, current_state, event.timestamp)
            }
            deck_event::Event::ScratchEnd => self.handle_scratch_end(),
            deck_event::Event::ResetPitch => self.update_pitch(PitchUpdate::Reset, current_pitch),
            deck_event::Event::SetPitch(pitch) => {
                self.update_pitch(PitchUpdate::Set(pitch), current_pitch)
            }
            deck_event::Event::PitchUp => {
                self.update_pitch(PitchUpdate::Adjust(0.01), current_pitch)
            }
            deck_event::Event::PitchDown => {
                self.update_pitch(PitchUpdate::Adjust(-0.01), current_pitch)
            }
            deck_event::Event::StartStop => self.start_or_stop(),
            deck_event::Event::LoadTrack(track) => log_try_send(
                &self.deck_worker,
                DeckWorkerEvent::LoadRecord(track),
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
