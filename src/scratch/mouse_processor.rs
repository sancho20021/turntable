use std::sync::{Arc, atomic::Ordering};

use sdl2::{event::Event, mouse::MouseButton};

use crate::scratch::{controller::ScratchController, mouse_analyzer::MovementRecorder};

pub struct MouseProcessor {
    controller: Arc<ScratchController>,
    recorder: MovementRecorder,
    smoothed_x: f64,
    /// Coefficient of smoothing of mouse movement events.
    /// 0 = no smoothing. 1 = maximum smoothing
    smoothing: f64,
}

impl MouseProcessor {
    pub fn new(controller: Arc<ScratchController>, smoothing: f64) -> Self {
        Self {
            controller,
            recorder: MovementRecorder::new(),
            smoothed_x: 0.,
            smoothing,
        }
    }

    pub fn handle_event(&mut self, event: sdl2::event::Event) {
        let ctrl = &self.controller;
        match event {
            Event::MouseMotion { x, .. } => {
                let mix = 1.0 - self.smoothing;
                self.smoothed_x += (x as f64 - self.smoothed_x) * mix;

                ctrl.current_x
                    .store(self.smoothed_x as i32, Ordering::Relaxed);

                self.recorder.record(self.smoothed_x);
            }

            Event::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                ..
            } => {
                self.smoothed_x = x as f64;

                ctrl.anchor_sample
                    .store(ctrl.playhead.load(Ordering::Relaxed), Ordering::Relaxed);
                ctrl.anchor_x.store(x, Ordering::Relaxed);
                ctrl.current_x.store(x, Ordering::Relaxed);
                ctrl.scratching.store(true, Ordering::Relaxed);

                self.recorder.record(self.smoothed_x);
            }

            Event::MouseButtonUp {
                mouse_btn: MouseButton::Left,
                ..
            } => {
                ctrl.scratching.store(false, Ordering::Relaxed);
                self.recorder.finish_motion();
            }

            Event::Quit { .. } => {
                println!("Mouse statistics:");
                self.recorder.analyze();
                return;
            }

            _ => {}
        }
    }
}
