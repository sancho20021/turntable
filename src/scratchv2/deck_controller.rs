use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use atomic_float::AtomicF64;
use crossbeam::{
    atomic::AtomicCell,
    channel::{Receiver, Sender, bounded},
};

use crate::{
    deck_event::DeckEvent,
    record::INanos,
    scratchv2::{
        platter_driver::{PlatterDriver, PlayheadUpdate},
        virtual_platter::{ReadablePlatter, WritablePlatter},
    },
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

/// Stateful Scratch controller that can be used to update virtual platter
/// based on the state which can be either normal playback or scratching mode.
#[derive(Debug)]
pub struct DeckController {
    state: Arc<DeckState>,
    change_record: Sender<String>,
    adjust_playhead: Sender<PlayheadUpdate>,
    platter: ReadablePlatter,
    // Receives events from outside senders
    event_receiver: Receiver<ExternalEvent>,
    // so the controller can clone and hand out senders
    event_sender: Sender<ExternalEvent>,
}

#[derive(Debug, Clone, Copy)]
enum PitchUpdate {
    Reset,
    Set(f64),
    Adjust(f64),
}

#[derive(Debug)]
pub enum ExternalEvent {
    RecordChanged,
}

static BASE_SENSITIVITY_FACTOR: f64 = 1_500_000.0;

impl DeckController {
    pub fn new(
        readable_platter: ReadablePlatter,
        writable_platter: WritablePlatter,
        record_changer: Sender<String>,
        initial_pitch: f64,
        sensitivity: f64,
        inertia_tau_secs: f64,
    ) -> (Self, PlatterDriver) {
        let initial_state = Arc::new(DeckState {
            pitch: AtomicF64::new(initial_pitch),
            playing: AtomicBool::new(true),
            platter: AtomicCell::new(PlatterState::Playing),
        });
        let (pl_snd, pl_rcv) = bounded(1000);

        let platter_src = PlatterDriver::new(
            Arc::clone(&initial_state),
            sensitivity * BASE_SENSITIVITY_FACTOR,
            inertia_tau_secs,
            writable_platter,
            pl_rcv,
        );
        let (event_snd, event_rcv) = bounded(100);
        (
            Self {
                state: initial_state,
                platter: readable_platter,
                change_record: record_changer,
                adjust_playhead: pl_snd,
                event_sender: event_snd,
                event_receiver: event_rcv,
            },
            platter_src,
        )
    }

    pub fn get_event_sender(&self) -> Sender<ExternalEvent> {
        self.event_sender.clone()
    }

    fn handle_mouse_motion(&self, x: i32, mut current: PlatterState) {
        if let PlatterState::Scratching {
            ref mut latest_mouse_x,
            ..
        } = current
        {
            *latest_mouse_x = x;
            self.state.platter.store(current);
        }
    }

    fn handle_mouse_down(&self, x: i32, current: PlatterState) {
        if let PlatterState::Playing = current {
            let scratch_state = PlatterState::Scratching {
                anchor_pos: self.platter.get_playhead().record_pos,
                anchor_mouse_x: x,
                latest_mouse_x: x,
            };
            self.state.platter.store(scratch_state);
        }
    }

    fn handle_mouse_up(&self) {
        self.state.platter.store(PlatterState::Playing);
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

    /// Starts a background thread that connects external components (like the **`RecordChanger`**) to this controller.
    ///
    /// **Must be called at startup**
    pub fn listen_to_external_events(self: Arc<Self>, shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
        std::thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
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

    fn process_external_event(&self, event: ExternalEvent) {
        match event {
            ExternalEvent::RecordChanged => {
                println!("Track loaded");
                log_try_send(
                    &self.adjust_playhead,
                    PlayheadUpdate::ToZero,
                    "reset playhead",
                )
            }
        }
    }

    pub fn handle_deck_event(&self, event: DeckEvent) -> anyhow::Result<()> {
        let current_pitch = self.state.pitch.load(Ordering::Relaxed);
        let current_state = self.state.platter.load();
        match event {
            DeckEvent::MouseMotion(x) => self.handle_mouse_motion(x, current_state),
            DeckEvent::MouseDown(x) => self.handle_mouse_down(x, current_state),
            DeckEvent::MouseUp(_) => self.handle_mouse_up(),
            DeckEvent::ResetPitch => self.update_pitch(PitchUpdate::Reset, current_pitch),
            DeckEvent::PitchUp => self.update_pitch(PitchUpdate::Adjust(0.01), current_pitch),
            DeckEvent::PitchDown => self.update_pitch(PitchUpdate::Adjust(-0.01), current_pitch),
            DeckEvent::StartStop => self.start_or_stop(),
            DeckEvent::LoadTrack(track) => {
                log_try_send(&self.change_record, track, "change record")
            }
            DeckEvent::PlayheadReset => log_try_send(
                &self.adjust_playhead,
                PlayheadUpdate::ToZero,
                "reset playhead",
            ),
            DeckEvent::PlayheadFF => log_try_send(
                &self.adjust_playhead,
                PlayheadUpdate::FastForward,
                "fast forward",
            ),
            DeckEvent::PlayheadRewind => {
                log_try_send(&self.adjust_playhead, PlayheadUpdate::Rewind, "rewind")
            }
        };
        Ok(())
    }
}
