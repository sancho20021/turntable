use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::scratchv2::{
    deck_controller::{ControllerState, PlatterState},
    virtual_platter::{INanos, PlatterSample, UNanos, WritablePlatter},
};

#[derive(Debug)]
pub struct PlatterSource {
    state: Arc<ControllerState>,
    sensitivity: f64,
    platter: WritablePlatter,
}

impl PlatterSource {
    pub fn new(state: Arc<ControllerState>, sensitivity: f64, platter: WritablePlatter) -> Self {
        Self {
            state,
            sensitivity,
            platter,
        }
    }

    /// Calculates platter position in nanos
    fn calculate_position(&self) -> PlatterSample {
        let speed = self.state.speed.load(Ordering::Relaxed);
        let state = self.state.platter.load();
        let now = self.platter.now();
        let cur_playhead = self.platter.get_playhead();
        match state {
            PlatterState::Playing => {
                if now <= cur_playhead.timestamp_nanos {
                    return cur_playhead;
                }
                let elapsed_nanos = UNanos(now.0 - cur_playhead.timestamp_nanos.0);

                // Position advances relative to elapsed time and playback speed
                let position_delta = (elapsed_nanos.0 as f64 * speed) as i64;
                PlatterSample {
                    timestamp_nanos: now,
                    record_pos: INanos(cur_playhead.record_pos.0 + position_delta),
                }
            }
            PlatterState::Scratching {
                anchor_pos: anchor_platter,
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
                    record_pos: INanos(anchor_platter.0 + position_delta),
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
