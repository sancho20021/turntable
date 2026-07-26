use std::sync::Arc;

use crossbeam::atomic::AtomicCell;

use crate::{
    deck_event::DeckEvent,
    scratchv2::{
        platter_driver::PlatterSource,
        virtual_platter::{PlatterSample, ReadablePlatter, WritablePlatter},
    },
};

const SPEED_EPS: f64 = 0.001;



#[derive(Clone, Copy, Debug)]
pub enum ControllerState {
    Playing {
        /// The playhead anchor when playback started/resumed
        start_sample: PlatterSample,
        /// Playback speed multiplier (1.0 = normal)
        speed: f64,
    },
    Scratching {
        /// The exact state of the virtual platter when the mouse went down
        anchor_platter: PlatterSample,
        /// The mouse X position when the mouse went down
        anchor_mouse_x: i32,
        /// The latest mouse position sent by the OS
        latest_mouse_x: i32,
        /// Previous speed before scratching started
        previous_speed: f64,
    },
}

/// Stateful Scratch controller that can be used to update virtual platter
/// based on the state which can be either normal playback or scratching mode.
#[derive(Debug)]
pub struct ScratchController {
    state: Arc<AtomicCell<ControllerState>>,
    platter: ReadablePlatter,
    previous_speed: f64,
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
        initial_speed: f64,
        sensitivity: f64,
    ) -> (Self, PlatterSource) {
        let initial_state = Arc::new(AtomicCell::new(ControllerState::Playing {
            start_sample: readable_platter.get_playhead(),
            speed: initial_speed,
        }));
        let platter_src = PlatterSource::new(
            Arc::clone(&initial_state),
            sensitivity * BASE_SENSITIVITY_FACTOR,
            writable_platter,
        );
        (
            Self {
                state: initial_state,
                platter: readable_platter,
                previous_speed: initial_speed,
            },
            platter_src,
        )
    }

    fn handle_mouse_motion(&self, x: i32, mut current: ControllerState) {
        if let ControllerState::Scratching {
            ref mut latest_mouse_x,
            ..
        } = current
        {
            *latest_mouse_x = x;
            self.state.store(current);
        }
    }

    fn handle_mouse_down(&self, x: i32, current: ControllerState) {
        if let ControllerState::Playing { speed, .. } = current {
            let scratch_state = ControllerState::Scratching {
                anchor_platter: self.platter.get_playhead(),
                anchor_mouse_x: x,
                latest_mouse_x: x,
                previous_speed: speed,
            };
            self.state.store(scratch_state);
        }
    }

    fn handle_mouse_up(&self, current: ControllerState) {
        if let ControllerState::Scratching { previous_speed, .. } = current {
            let play_state = ControllerState::Playing {
                start_sample: self.platter.get_playhead(),
                speed: previous_speed,
            };
            self.state.store(play_state);
        }
    }

    /// update speed in controller state but dont touch speed_copy
    fn update_speed(&mut self, update: SpeedUpdate, current: ControllerState) {
        if let ControllerState::Playing { speed, .. } = current {
            let new_speed = match update {
                SpeedUpdate::Reset => 1.,
                SpeedUpdate::Set(x) => x,
                SpeedUpdate::Adjust(delta) => speed + delta,
            };

            // Because the playhead is calculated relatively to some anchor position, we have to update the anchor with the latest playhead.
            // Otherwise playhead will jump
            let cur_playhead = self.platter.get_playhead();
            self.state.store(ControllerState::Playing {
                start_sample: cur_playhead,
                speed: new_speed,
            });
        }
    }

    fn start_or_stop(&mut self, current: ControllerState) {
        if let ControllerState::Playing { speed, .. } = current {
            if speed.abs() < SPEED_EPS {
                // we consider the deck was still
                self.update_speed(SpeedUpdate::Set(self.previous_speed), current);
            } else {
                // the deck was playing
                self.previous_speed = speed;
                self.update_speed(SpeedUpdate::Set(0.), current);
            }
        }
    }

    pub fn handle_deck_event(&mut self, event: DeckEvent) {
        let current = self.state.load();
        match event {
            DeckEvent::MouseMotion(x) => self.handle_mouse_motion(x, current),
            DeckEvent::MouseDown(x) => self.handle_mouse_down(x, current),
            DeckEvent::MouseUp(_) => self.handle_mouse_up(current),
            DeckEvent::KeyReset => self.update_speed(SpeedUpdate::Reset, current),
            DeckEvent::KeyUp => self.update_speed(SpeedUpdate::Adjust(0.01), current),
            DeckEvent::KeyDown => self.update_speed(SpeedUpdate::Adjust(-0.01), current),
            DeckEvent::StartStop => self.start_or_stop(current),
        }
    }
}
