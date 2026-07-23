use std::sync::Arc;

use crossbeam::atomic::AtomicCell;

use crate::{
    deck_event::DeckEvent,
    scratchv2::virtual_platter::{INanos, PlatterSample, UNanos, VirtualPlatter},
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
#[derive(Debug, Clone)]
pub struct ScratchController {
    state: Arc<AtomicCell<ControllerState>>,
    platter: VirtualPlatter,
    sensitivity: f64,
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
    pub fn new(platter: VirtualPlatter, initial_speed: f64, sensitivity: f64) -> Self {
        let initial_state = ControllerState::Playing {
            start_sample: platter.get_playhead(),
            speed: initial_speed,
        };
        Self {
            state: Arc::new(AtomicCell::new(initial_state)),
            sensitivity,
            platter,
            previous_speed: initial_speed,
        }
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

    /// Calculates platter position in nanos
    fn calculate_position(&self) -> PlatterSample {
        let state = self.state.load();
        let now = self.platter.now();
        match state {
            ControllerState::Playing {
                start_sample,
                speed,
            } => {
                if now <= start_sample.timestamp_nanos {
                    return start_sample;
                }
                let elapsed_nanos = UNanos(now.0 - start_sample.timestamp_nanos.0);

                // Position advances relative to elapsed time and playback speed
                let position_delta = (elapsed_nanos.0 as f64 * speed) as i64;
                PlatterSample {
                    timestamp_nanos: now,
                    record_pos: INanos(start_sample.record_pos.0 + position_delta),
                }
            }
            ControllerState::Scratching {
                anchor_platter,
                anchor_mouse_x,
                latest_mouse_x,
                ..
            } => {
                // TODO: in mouse updates save timestamps as well because mouse updates can be older than now
                let mouse_delta = (latest_mouse_x - anchor_mouse_x) as f64;

                // Map mouse movement straight to playhead offset
                let position_delta =
                    (mouse_delta * self.sensitivity * BASE_SENSITIVITY_FACTOR) as i64;
                PlatterSample {
                    timestamp_nanos: now,
                    record_pos: INanos(anchor_platter.record_pos.0 + position_delta),
                }
            }
        }
    }

    /// Updates virtual platter according to current state
    pub fn update_platter(&self) {
        let pos = self.calculate_position();
        self.platter
            .update_playhead(pos.record_pos, pos.timestamp_nanos);
    }
}
