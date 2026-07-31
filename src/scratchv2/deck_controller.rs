use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use atomic_float::AtomicF64;
use crossbeam::{atomic::AtomicCell, channel::Sender};

use crate::{
    deck_event::DeckEvent,
    decoder::load_file,
    record::{INanos, Record, interpolation::Interpolator},
    scratchv2::{
        platter_driver::PlatterSource,
        virtual_platter::{ReadablePlatter, WritablePlatter},
    },
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
    change_record: Sender<Record>,
    platter: ReadablePlatter,
}

#[derive(Debug, Clone, Copy)]
enum PitchUpdate {
    Reset,
    Set(f64),
    Adjust(f64),
}

// 1.0 sensitivity means 400 pixels = 1 second of audio
static BASE_SENSITIVITY_FACTOR: f64 = 2_500_000.0;

impl DeckController {
    pub fn new(
        readable_platter: ReadablePlatter,
        writable_platter: WritablePlatter,
        record_sender: Sender<Record>,
        initial_pitch: f64,
        sensitivity: f64,
        inertia_tau_secs: f64,
    ) -> (Self, PlatterSource) {
        let initial_state = Arc::new(DeckState {
            pitch: AtomicF64::new(initial_pitch),
            playing: AtomicBool::new(true),
            platter: AtomicCell::new(PlatterState::Playing),
        });
        let platter_src = PlatterSource::new(
            Arc::clone(&initial_state),
            sensitivity * BASE_SENSITIVITY_FACTOR,
            inertia_tau_secs,
            writable_platter,
        );
        (
            Self {
                state: initial_state,
                platter: readable_platter,
                change_record: record_sender,
                // send_events:
            },
            platter_src,
        )
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

    fn update_pitch(&mut self, update: PitchUpdate, cur_speed: f64) {
        let new_speed = match update {
            PitchUpdate::Reset => 1.,
            PitchUpdate::Set(x) => x,
            PitchUpdate::Adjust(delta) => cur_speed + delta,
        };
        self.state.pitch.store(new_speed, Ordering::Relaxed);
    }

    fn start_or_stop(&mut self) {
        let playing = self.state.playing.load(Ordering::Relaxed);
        self.state.playing.store(!playing, Ordering::Relaxed);
    }

    pub fn handle_deck_event(&mut self, event: DeckEvent) -> anyhow::Result<()> {
        let current_pitch = self.state.pitch.load(Ordering::Relaxed);
        let current_state = self.state.platter.load();
        match event {
            DeckEvent::MouseMotion(x) => self.handle_mouse_motion(x, current_state),
            DeckEvent::MouseDown(x) => self.handle_mouse_down(x, current_state),
            DeckEvent::MouseUp(_) => self.handle_mouse_up(),
            DeckEvent::KeyReset => self.update_pitch(PitchUpdate::Reset, current_pitch),
            DeckEvent::KeyUp => self.update_pitch(PitchUpdate::Adjust(0.01), current_pitch),
            DeckEvent::KeyDown => self.update_pitch(PitchUpdate::Adjust(-0.01), current_pitch),
            DeckEvent::StartStop => self.start_or_stop(),
            DeckEvent::LoadTrack(track) => {
                let send_rec = self.change_record.clone();
                println!("Loading: {}", track);
                std::thread::spawn(move || {
                    let rec = load_file(44100, track.as_ref());
                    match rec {
                        Ok(rec) => {
                            let rec = Record::new(rec, Interpolator::linear(), 44100);

                            match send_rec.try_send(rec) {
                                Ok(()) => {}
                                Err(e) => {
                                    log::error!("failed to change record, try again: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("failed to load track: {e}");
                        }
                    }
                });
            }
        };
        Ok(())
    }
}
