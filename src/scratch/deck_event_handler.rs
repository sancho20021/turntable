use std::{
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use crate::scratch::{
    mouse_analyzer::MovementRecorder,
    shared_state::{MouseUpdate, PlatterState, ScratchState},
};

/// Handles DeckEvents and updates the deck state
pub struct DeckEventHandler {
    scratch_data: Arc<ScratchState>,
    recorder: MovementRecorder,
}

#[derive(Debug, Clone, Copy)]
pub enum DeckEvent {
    MouseMotion(i32),
    MouseDown(i32),
    MouseUp(i32),
    KeyReset,
    KeyUp,
    KeyDown,
}

impl DeckEventHandler {
    pub fn new(scratch_data: Arc<ScratchState>, recorder: MovementRecorder) -> Self {
        Self {
            scratch_data,
            recorder,
        }
    }

    pub fn set_recorder_start(&mut self, time: Instant) {
        self.recorder.start = time;
    }

    pub fn handle_event(&mut self, event: DeckEvent) {
        let ctrl = &self.scratch_data;
        match event {
            DeckEvent::MouseMotion(x) => {
                let time = Instant::now();
                ctrl.latest_mouse.store(MouseUpdate {
                    pos: x as f64,
                    when: time,
                });

                self.recorder.record(x as f64);
            }
            DeckEvent::MouseDown(x) => {
                let time = Instant::now();
                ctrl.platter.store(PlatterState::Scratching {
                    anchor_sample: ctrl.playhead.load(Ordering::Relaxed),
                    anchor_mouse: x,
                    start_time: time,
                });
                ctrl.latest_mouse.store(MouseUpdate {
                    pos: x as f64,
                    when: time,
                });

                self.recorder.record(x as f64);
            }
            DeckEvent::MouseUp(_) => {
                ctrl.platter.store(PlatterState::Playing);
            }
            DeckEvent::KeyReset => {
                println!("Speed reset");
                ctrl.speed.store(1., Ordering::Relaxed);
            }
            DeckEvent::KeyUp => {
                let speed = ctrl.speed.load(Ordering::Relaxed) + 0.01;
                print!("Speed := {:.0}%", speed * 100.0);
                ctrl.speed.store(speed, Ordering::Relaxed);
            }
            DeckEvent::KeyDown => {
                let speed = ctrl.speed.load(Ordering::Relaxed) - 0.01;
                print!("Speed := {:.0}%", speed * 100.0);
                ctrl.speed.store(speed, Ordering::Relaxed);
            }
        }
    }

    pub fn stop_recorder(&mut self) {
        self.recorder.finish();
    }
}
