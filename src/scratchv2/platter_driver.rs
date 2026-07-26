use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam::atomic::AtomicCell;

use crate::scratchv2::{
    deck_controller::ControllerState,
    virtual_platter::{INanos, PlatterSample, UNanos, WritablePlatter},
};

#[derive(Debug)]
pub struct PlatterSource {
    state: Arc<AtomicCell<ControllerState>>,
    sensitivity: f64,
    platter: WritablePlatter,
}

impl PlatterSource {
    pub fn new(
        state: Arc<AtomicCell<ControllerState>>,
        sensitivity: f64,
        platter: WritablePlatter,
    ) -> Self {
        Self {
            state,
            sensitivity,
            platter,
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
                let position_delta = (mouse_delta * self.sensitivity) as i64;
                PlatterSample {
                    timestamp_nanos: now,
                    record_pos: INanos(anchor_platter.record_pos.0 + position_delta),
                }
            }
        }
    }

    /// Updates virtual platter according to current state
    pub fn update_platter(&mut self) {
        let pos = self.calculate_position();
        self.platter
            .update_playhead(pos.record_pos, pos.timestamp_nanos);
    }
}

pub fn spawn_platter_driver(
    mut platter_src: PlatterSource,
    update_frequency_hz: f64,
    shutdown_flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let interval = Duration::from_secs_f64(1.0 / update_frequency_hz);

        while !shutdown_flag.load(Ordering::Relaxed) {
            let loop_start = Instant::now();
            platter_src.update_platter();
            // 5. High-precision sleep to maintain targeted update frequency
            let elapsed = loop_start.elapsed();
            if elapsed < interval {
                std::thread::sleep(interval - elapsed);
            }
        }

        println!("Platter stopped");
    })
}
