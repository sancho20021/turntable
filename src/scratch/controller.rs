use atomic_float::AtomicF64;
use sdl2::event::Event;
use sdl2::mouse::MouseButton;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::scratch::mouse_analyzer::MovementRecorder;
use crate::scratch::mouse_processor::MouseProcessor;

/// Maps touchpad X position into a scratch region of audio.
pub struct ScratchController {
    /// virtual playhead. Current sample
    pub playhead: AtomicF64,
    pub scratching: AtomicBool,
    /// sample where scratching started
    pub anchor_sample: AtomicF64,
    /// touchpad / platter position where scratching started
    pub anchor_x: AtomicI32,
    /// current touchpad / platter position
    pub current_x: AtomicI32,
    /// how many samples should touchpad delta x increment
    pub sensitivity: f64,
}

impl ScratchController {
    pub fn new(playhead: f64, sensitivity: f64) -> Self {
        Self {
            playhead: AtomicF64::new(playhead),
            scratching: AtomicBool::new(false),
            anchor_sample: AtomicF64::new(playhead),
            anchor_x: AtomicI32::new(0),
            current_x: AtomicI32::new(0),
            sensitivity,
        }
    }
}

pub fn spawn_scratch_input(mut mouse_processor: MouseProcessor) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let sdl = sdl2::init().unwrap();
        let video = sdl.video().unwrap();

        let _window = video
            .window("scratch input", 300, 100)
            .position_centered()
            .build()
            .unwrap();

        let mut pump = sdl.event_pump().unwrap();

        for event in pump.wait_iter() {
            mouse_processor.handle_event(event.clone());
            if let Event::Quit { .. } = event {
                return;
            }
        }
    })
}
