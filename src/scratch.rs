use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use sdl2::event::Event;
use sdl2::mouse::MouseButton;

/// Maps touchpad X position into a scratch region of audio.
pub struct ScratchController {
    pub x: AtomicI32,
    pub touching: AtomicBool,

    pub max_x: i32,

    pub start_sec: f64,
    pub end_sec: f64,
}

impl ScratchController {
    pub fn new(max_x: i32, start_sec: f64, end_sec: f64) -> Self {
        Self {
            x: AtomicI32::new(0),
            touching: AtomicBool::new(false),
            max_x,
            start_sec,
            end_sec,
        }
    }

    /// normalized position 0..1
    pub fn norm(&self) -> f64 {
        let x = self.x.load(Ordering::Relaxed).clamp(0, self.max_x);
        x as f64 / self.max_x as f64
    }

    /// mapped playback position in seconds (50 → 55)
    pub fn time(&self) -> f64 {
        let t = self.norm();
        self.start_sec + (self.end_sec - self.start_sec) * t
    }

    /// whether user is actively scratching
    pub fn touching(&self) -> bool {
        self.touching.load(Ordering::Relaxed)
    }
}

pub fn spawn_scratch_input(ctrl: Arc<ScratchController>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let sdl = sdl2::init().unwrap();
        let video = sdl.video().unwrap();

        let _window = video
            .window("scratch input", 300, 100)
            .position_centered()
            .build()
            .unwrap();

        let mut pump = sdl.event_pump().unwrap();

        loop {
            for event in pump.poll_iter() {
                match event {
                    Event::MouseMotion { x, .. } => {
                        ctrl.x.store(x, Ordering::Relaxed);
                    }

                    Event::MouseButtonDown {
                        mouse_btn: MouseButton::Left,
                        ..
                    } => {
                        ctrl.touching.store(true, Ordering::Relaxed);
                    }

                    Event::MouseButtonUp {
                        mouse_btn: MouseButton::Left,
                        ..
                    } => {
                        ctrl.touching.store(false, Ordering::Relaxed);
                    }

                    Event::Quit { .. } => return,

                    _ => {}
                }
            }

            std::thread::sleep(Duration::from_millis(1));
        }
    })
}

fn main() {
    
}
