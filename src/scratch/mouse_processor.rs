use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use sdl2::{event::Event, keyboard::Keycode, mouse::MouseButton};

use crate::scratch::{mouse_analyzer::MovementRecorder, shared_state::ScratchState};

/// Translates mouse events to Scratch state
/// But also currently responsible for quitting the window and finishing all monitors
pub struct MouseProcessor {
    scratch_data: Arc<ScratchState>,
    recorder: MovementRecorder,
    deck_monitor_shutdown: Arc<AtomicBool>,
}

impl MouseProcessor {
    pub fn new(
        scratch_data: Arc<ScratchState>,
        recorder: MovementRecorder,
        deck_monitor_shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            scratch_data,
            recorder,
            deck_monitor_shutdown,
        }
    }

    pub fn handle_event(&mut self, event: sdl2::event::Event) {
        let ctrl = &self.scratch_data;
        match event {
            Event::MouseMotion { x, .. } => {
                ctrl.current_x.store(x, Ordering::Relaxed);

                self.recorder.record(x as f64);
            }

            Event::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                ..
            } => {
                ctrl.anchor_sample
                    .store(ctrl.playhead.load(Ordering::Relaxed), Ordering::Relaxed);
                ctrl.anchor_x.store(x, Ordering::Relaxed);
                ctrl.current_x.store(x, Ordering::Relaxed);
                ctrl.scratching.store(true, Ordering::Relaxed);

                self.recorder.record(x as f64);
            }

            Event::MouseButtonUp {
                mouse_btn: MouseButton::Left,
                ..
            } => {
                ctrl.scratching.store(false, Ordering::Relaxed);
            }

            Event::KeyDown { keycode, .. } => {
                if let Some(key) = keycode {
                    match key {
                        Keycode::R => {
                            println!("Speed reset");
                            ctrl.speed.store(1., Ordering::Relaxed);
                        }
                        Keycode::Up => {
                            let speed = ctrl.speed.load(Ordering::Relaxed) + 0.01;
                            print!("Speed := {:.0}%", speed * 100.0);
                            ctrl.speed.store(speed, Ordering::Relaxed);
                        }
                        Keycode::Down => {
                            let speed = ctrl.speed.load(Ordering::Relaxed) - 0.01;
                            print!("Speed := {:.0}%", speed * 100.0);
                            ctrl.speed.store(speed, Ordering::Relaxed);
                        }
                        _ => (),
                    }
                }
            }

            Event::Quit { .. } => {
                println!("Mouse statistics:");
                self.recorder.finish();
                self.deck_monitor_shutdown.store(true, Ordering::Relaxed);
                return;
            }

            _ => {}
        }
    }
}
