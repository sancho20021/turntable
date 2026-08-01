use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam::channel::Receiver;

use crate::{
    record::{INanos, UNanos},
    scratchv2::{
        deck_controller::{DeckState, PlatterState},
        physical_speed::Speed,
        virtual_platter::{PlatterSample, WritablePlatter},
    },
};

/// time that fast-forward skips
static FF_TIME: UNanos = UNanos(15 * 1_000_000_000);

#[derive(Debug)]
pub enum PlayheadUpdate {
    /// set playhead to the start
    ToZero,
    /// skip FF_TIME forward
    FastForward,
    /// go FF_TIME backward
    Rewind,
}

#[derive(Debug)]
pub struct PlatterDriver {
    state: Arc<DeckState>,
    motor_speed: Speed,
    record_speed: Speed,
    sensitivity: f64,
    platter: WritablePlatter,
    playhead_events: Receiver<PlayheadUpdate>,
}

impl PlatterDriver {
    pub fn new(
        state: Arc<DeckState>,
        sensitivity: f64,
        inertia_tau_secs: f64,
        platter: WritablePlatter,
        playhead_events: Receiver<PlayheadUpdate>,
    ) -> Self {
        let initial_speed = state.target_speed();
        let motor_speed = Speed::new(inertia_tau_secs, 0.1, initial_speed);
        // I want the record to sync with the platter in about 50ms after scratching
        let record_speed = Speed::new(0.01, 25., initial_speed);
        Self {
            state,
            motor_speed,
            record_speed: record_speed,
            sensitivity,
            platter,
            playhead_events,
        }
    }

    /// Calculates platter position in nanos
    fn calculate_position(&mut self) -> PlatterSample {
        let state = self.state.platter.load();
        let now = self.platter.now();
        let cur_playhead = self.platter.get_playhead();

        match state {
            PlatterState::Playing => {
                if now <= cur_playhead.timestamp_nanos {
                    return cur_playhead;
                }
                let elapsed_nanos = UNanos(now.0 - cur_playhead.timestamp_nanos.0);

                let speed = {
                    let dt_secs = elapsed_nanos.0 as f64 / 1_000_000_000.;
                    // motor speed tries reaching pitch
                    self.motor_speed
                        .advance_speed(dt_secs, self.state.target_speed());
                    // record speed catches up with motor speed (not instant because of virtual slipmat)
                    self.record_speed
                        .advance_speed(dt_secs, self.motor_speed.get());
                    self.record_speed.get()
                };

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
                // TODO:
                // inspect mouse updates (estimated speed, and check why it scratch release doesn't behave as expected)
                //
                // TODO: in mouse updates save timestamps as well because mouse updates can be older than now
                let mouse_delta = (latest_mouse_x - anchor_mouse_x) as f64;

                // Map mouse movement straight to playhead offset
                let position_delta = (mouse_delta * self.sensitivity) as i64;
                let new_sample = PlatterSample {
                    timestamp_nanos: now,
                    record_pos: INanos(anchor_platter.0 + position_delta),
                };
                if new_sample.timestamp_nanos > cur_playhead.timestamp_nanos {
                    // todo: separate inertia of "scratch release and motor inertia"
                    const TAU_NANOS: f64 = 10_000_000.0;
                    let dt_nanos =
                        (new_sample.timestamp_nanos.0 - cur_playhead.timestamp_nanos.0) as f64;
                    let factor = (-dt_nanos / TAU_NANOS).exp();
                    let raw_speed =
                        (new_sample.record_pos.0 - cur_playhead.record_pos.0) as f64 / dt_nanos;
                    // Applying frequency-invariant exponential smoothing
                    let current_speed = self.record_speed.get();
                    let next_speed = raw_speed + (current_speed - raw_speed) * factor;
                    self.record_speed.hard_set_speed(next_speed);
                }
                new_sample
            }
        }
    }

    fn adjust_playhead(&mut self, adjust: PlayheadUpdate) {
        let cur_playhead = self.platter.get_playhead().record_pos;
        let now = self.platter.now();
        let new_pos = INanos(match adjust {
            PlayheadUpdate::ToZero => 0,
            PlayheadUpdate::FastForward => cur_playhead.0 + FF_TIME.0 as i64,
            PlayheadUpdate::Rewind => cur_playhead.0 - FF_TIME.0 as i64,
        });
        self.platter.update_playhead(new_pos, now);
    }

    /// Updates virtual platter according to current state
    pub fn update_platter(&mut self) {
        // to not get stuck handling updates
        for _ in 0..1000 {
            if let Ok(upd) = self.playhead_events.try_recv() {
                self.adjust_playhead(upd);
            }
        }
        let pos = self.calculate_position();
        self.platter
            .update_playhead(pos.record_pos, pos.timestamp_nanos);
    }

    pub fn start(
        mut self,
        update_frequency_hz: f64,
        shutdown_flag: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let interval = Duration::from_secs_f64(1.0 / update_frequency_hz);

            while !shutdown_flag.load(Ordering::Relaxed) {
                let loop_start = Instant::now();
                self.update_platter();
                // 5. High-precision sleep to maintain targeted update frequency
                let elapsed = loop_start.elapsed();
                if elapsed < interval {
                    std::thread::sleep(interval - elapsed);
                }
            }

            println!("Platter stopped");
        })
    }
}
