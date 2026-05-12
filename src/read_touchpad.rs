use sdl2::event::Event;
use sdl2::mouse::MouseButton;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use std::{sync::Arc, thread};

use crate::touchpad_state::TouchpadState;

pub fn read_touchpad(state: Arc<TouchpadState>) -> impl Fn() {
    move || {
        let sdl = sdl2::init().unwrap();
        let video = sdl.video().unwrap();

        let _window = video
            .window("input thread", 200, 200)
            .position_centered()
            .build()
            .unwrap();

        let mut pump = sdl.event_pump().unwrap();

        loop {
            for event in pump.poll_iter() {
                match event {
                    Event::MouseMotion { x: mx, .. } => {
                        state.x.store(mx, Ordering::Relaxed);
                    }

                    Event::MouseButtonDown {
                        mouse_btn: MouseButton::Left,
                        ..
                    } => {
                        state.touching.store(true, Ordering::Relaxed);
                    }

                    Event::MouseButtonUp {
                        mouse_btn: MouseButton::Left,
                        ..
                    } => {
                        state.touching.store(false, Ordering::Relaxed);
                    }

                    Event::Quit { .. } => return,

                    _ => {}
                }
            }

            thread::sleep(Duration::from_millis(1));
        }
    }
}

pub fn main() {
    let state = Arc::new(TouchpadState::new());

    let _handle = std::thread::spawn(read_touchpad(state.clone()));

    loop {
        if state.touching.load(std::sync::atomic::Ordering::Relaxed) {
            let x = state.x.load(std::sync::atomic::Ordering::Relaxed);
            println!("x = {}", x);
        }

        thread::sleep(Duration::from_millis(100));
    }
}
