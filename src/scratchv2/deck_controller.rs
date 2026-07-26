use std::sync::{Arc, atomic::Ordering};

use atomic_float::AtomicF64;
use crossbeam::atomic::AtomicCell;

use crate::{
    deck_event::DeckEvent,
    scratchv2::{
        platter_driver::PlatterSource,
        virtual_platter::{INanos, ReadablePlatter, WritablePlatter},
    },
};

const SPEED_EPS: f64 = 0.001;

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
pub struct ControllerState {
    /// Current platter speed
    pub speed: AtomicF64,
    /// Platter state (scratching or playing)
    pub platter: AtomicCell<PlatterState>,
}

/// Stateful Scratch controller that can be used to update virtual platter
/// based on the state which can be either normal playback or scratching mode.
#[derive(Debug)]
pub struct ScratchController {
    /// Playback target speed (1.0 = normal)
    pub pitch: f64,
    state: Arc<ControllerState>,
    platter: ReadablePlatter,
}

#[derive(Debug, Clone, Copy)]
enum SpeedUpdate {
    Reset,
    Set(f64),
    Adjust(f64),
}

// 1.0 sensitivity means 400 pixels = 1 second of audio
static BASE_SENSITIVITY_FACTOR: f64 = 2_500_000.0;

impl ScratchController {
    pub fn new(
        readable_platter: ReadablePlatter,
        writable_platter: WritablePlatter,
        initial_pitch: f64,
        sensitivity: f64,
    ) -> (Self, PlatterSource) {
        let initial_state = Arc::new(ControllerState {
            speed: AtomicF64::new(0.),
            platter: AtomicCell::new(PlatterState::Playing),
        });
        let platter_src = PlatterSource::new(
            Arc::clone(&initial_state),
            sensitivity * BASE_SENSITIVITY_FACTOR,
            writable_platter,
        );
        (
            Self {
                state: initial_state,
                platter: readable_platter,
                pitch: initial_pitch,
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

    fn update_speed(&mut self, update: SpeedUpdate, cur_speed: f64) {
        let new_speed = match update {
            SpeedUpdate::Reset => 1.,
            SpeedUpdate::Set(x) => x,
            SpeedUpdate::Adjust(delta) => cur_speed + delta,
        };
        self.state.speed.store(new_speed, Ordering::Relaxed);
    }

    fn start_or_stop(&mut self, speed: f64) {
        if speed.abs() < SPEED_EPS {
            // we consider the deck was still
            self.update_speed(SpeedUpdate::Set(self.pitch), speed);
        } else {
            // the deck was playing
            self.update_speed(SpeedUpdate::Set(0.), speed);
        }
    }

    pub fn handle_deck_event(&mut self, event: DeckEvent) {
        let current_speed = self.state.speed.load(Ordering::Relaxed);
        let current_state = self.state.platter.load();
        match event {
            DeckEvent::MouseMotion(x) => self.handle_mouse_motion(x, current_state),
            DeckEvent::MouseDown(x) => self.handle_mouse_down(x, current_state),
            DeckEvent::MouseUp(_) => self.handle_mouse_up(),
            DeckEvent::KeyReset => self.update_speed(SpeedUpdate::Reset, current_speed),
            DeckEvent::KeyUp => self.update_speed(SpeedUpdate::Adjust(0.01), current_speed),
            DeckEvent::KeyDown => self.update_speed(SpeedUpdate::Adjust(-0.01), current_speed),
            DeckEvent::StartStop => self.start_or_stop(current_speed),
        }
    }
}
