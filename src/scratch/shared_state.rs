use atomic_float::AtomicF64;
use sdl2::event::Event;
use std::sync::atomic::{AtomicBool, AtomicI32};

use crate::scratch::mouse_processor::MouseProcessor;

/// Communication channel between touchpad and scratching engine
pub struct ScratchState {
    /// virtual playhead. Current sample
    pub playhead: AtomicF64,
    pub scratching: AtomicBool,
    /// sample where scratching started
    pub anchor_sample: AtomicF64,
    /// touchpad / platter position where scratching started
    pub anchor_x: AtomicI32,
    /// current touchpad / platter position
    pub current_x: AtomicI32,
    /// playback speed
    pub speed: AtomicF64,
}

impl ScratchState {
    pub fn new(playhead: f64) -> Self {
        Self {
            playhead: AtomicF64::new(playhead),
            scratching: AtomicBool::new(false),
            anchor_sample: AtomicF64::new(playhead),
            anchor_x: AtomicI32::new(0),
            current_x: AtomicI32::new(0),
            speed: AtomicF64::new(1.),
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
