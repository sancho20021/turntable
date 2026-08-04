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
    record_mouse,
    scratchv2::{
        deck_controller::{DeckState, PlatterState},
        physical_speed::Speed,
        virtual_platter::{PlatterSample, WritablePlatter},
    },
    telemetry::TelemetryTrace,
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
    record_speed: Speed,
    sensitivity: f64,
    platter: WritablePlatter,
    playhead_events: Receiver<PlayheadUpdate>,
    /// for recording metrics
    pub tracer: TelemetryTrace,
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
        let record_speed = Speed::new(inertia_tau_secs, 1., initial_speed);
        Self {
            state,
            record_speed,
            sensitivity,
            platter,
            playhead_events,
            tracer: TelemetryTrace::new(),
        }
    }

    /// Calculates platter position in nanos
    fn calculate_position(&mut self) -> PlatterSample {
        let state = self.state.platter.load();
        let now = self.platter.now();

        let cur_playhead = self.platter.get_playhead();

        let sample = match state {
            PlatterState::Playing => {
                if now <= cur_playhead.timestamp_nanos {
                    return cur_playhead;
                }
                let elapsed_nanos = UNanos(now.0 - cur_playhead.timestamp_nanos.0);

                let speed = {
                    let dt_secs = elapsed_nanos.0 as f64 / 1_000_000_000.;
                    // motor speed tries reaching pitch
                    self.record_speed
                        .advance_speed(dt_secs, self.state.target_speed());
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
                timestamp: latest_mouse_t,
                mouse_speed,
            } => {
                // TODO: scratch and stop leads to indefinite calculation below which is like lo-fi normal playback at
                // latest mouse speed.
                // probably can be fixed with
                // or some smart filter that if distance is too far away then converges to latest position?
                // but not sure. maybe delay time so that we take older value as current so that if we go
                // over the latest mouse event we stop and never extrapolate

                let cur_mouse: f64 = {
                    // we go 2ms in past to extrapolate less
                    let dt_secs: f64 = (now.0 - 2_000_000 - latest_mouse_t.0) as f64 / 1_000_000_000.;

                    // 1. Calculate where the mouse *would* be if it kept moving
                    let extrapolated_mouse = {
                        // for extrapolation we clamp dt
                        let dt_secs = dt_secs.clamp(-20. / 1_000., 10. / 1_000.);

                        let raw_extrapolated: f64 = match mouse_speed {
                            Some(speed) => latest_mouse_x as f64 + (speed * dt_secs),
                            None => latest_mouse_x as f64,
                        };

                        let max_drift_pixels = 50.;
                        raw_extrapolated.clamp(
                            latest_mouse_x as f64 - max_drift_pixels,
                            latest_mouse_x as f64 + max_drift_pixels,
                        )
                    };

                    record_mouse!(self.tracer, now, "extrapolated_mouse_x", extrapolated_mouse);

                    // 2. Convergence factor (higher lambda = snaps faster, lower = smoother/more inertia)
                    let lambda = 50.0;
                    let convergence_weight = (-lambda * dt_secs).exp(); // Drops from 1.0 to 0.0 over time

                    // 3. Blend between extrapolation (short-term) and the hard target (long-term)
                    (extrapolated_mouse * convergence_weight)
                        + (latest_mouse_x as f64 * (1.0 - convergence_weight))
                };

                record_mouse!(self.tracer, now, "converged_mouse_x", cur_mouse);

                let mouse_delta = cur_mouse - anchor_mouse_x as f64;

                // Map mouse movement to playhead offset
                let position_delta = (mouse_delta * self.sensitivity) as i64;
                let new_sample = PlatterSample {
                    timestamp_nanos: now,
                    record_pos: INanos(anchor_platter.0 + position_delta),
                };
                new_sample
            }
        };
        sample
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
        update_frequency_hz: usize,
        shutdown_flag: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Self> {
        std::thread::spawn(move || {
            let interval = Duration::from_secs_f64(1.0 / update_frequency_hz as f64);

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
            self
        })
    }
}
