use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam::channel::Receiver;

use crate::{
    deck_event::Direction,
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
pub enum Jump {
    /// set playhead to the start
    ToZero,
    /// skip FF_TIME forward
    Forward,
    /// go FF_TIME backward
    Backward,
}

#[derive(Debug)]
pub enum PlatterEvent {
    /// move playhead
    MovePlayhead(Jump),
    /// pitch bend / nudge
    Nudge(Direction),
}

struct NudgeQueue {
    /// nudge responsiveness
    responsiveness: f32,
    /// forward nudges, sorted from oldest to newest
    forward: VecDeque<Instant>,
    /// backward nudges, sorted from oldest to newest
    backward: VecDeque<Instant>,
}

impl NudgeQueue {
    pub fn new(responsiveness: f32) -> Self {
        Self {
            responsiveness: responsiveness * 2.,
            forward: Default::default(),
            backward: Default::default(),
        }
    }

    fn current_nudge(&mut self) -> f32 {
        let now = Instant::now();
        let lifetime = Duration::from_millis(100);

        // 1. Drain expired forward nudges one-by-one from the front
        while let Some(oldest_time) = self.forward.front() {
            if now.duration_since(*oldest_time) >= lifetime {
                self.forward.pop_front(); // Cleanly pop index 0
            } else {
                break;
            }
        }

        // 2. Drain expired backward nudges one-by-one from the front
        while let Some(oldest_time) = self.backward.front() {
            if now.duration_since(*oldest_time) >= lifetime {
                self.backward.pop_front(); // Cleanly pop index 0
            } else {
                break;
            }
        }

        self.responsiveness * (self.forward.len() as f32 - self.backward.len() as f32)
    }
}

pub struct PlatterDriver {
    state: Arc<DeckState>,
    record_speed: Speed,
    sensitivity: f64,
    platter: WritablePlatter,
    events: Receiver<PlatterEvent>,
    nudges: NudgeQueue,
    /// for recording metrics
    pub tracer: TelemetryTrace,
}

impl PlatterDriver {
    pub fn new(
        state: Arc<DeckState>,
        sensitivity: f64,
        inertia_tau_secs: f64,
        platter: WritablePlatter,
        events: Receiver<PlatterEvent>,
        nudge_responsiveness: f32,
    ) -> Self {
        let record_speed = Speed::new(inertia_tau_secs, 0.005);
        Self {
            state,
            record_speed,
            sensitivity,
            platter,
            events,
            tracer: TelemetryTrace::new(),
            nudges: NudgeQueue::new(nudge_responsiveness),
        }
    }

    /// Calculates platter position in nanos
    fn calculate_position(&mut self) -> PlatterSample {
        let state = self.state.platter.load();
        let now = self.platter.now();

        let cur_playhead = self.platter.get_playhead();
        let elapsed_nanos: f64 = (now.0 as f64 - cur_playhead.timestamp_nanos.0 as f64).max(0.);

        let target_speed = {
            let nudge_raw = self.nudges.current_nudge() as f64;
            let nudge_modifier = nudge_raw.clamp(-8., 8.) / 100.;
            self.state.target_speed() + nudge_modifier
        };

        let speed = self
            .record_speed
            .advance(elapsed_nanos / 1_000_000_000., target_speed);

        let sample = match state {
            PlatterState::Playing => {
                // Position advances relative to elapsed time and playback speed
                let position_delta = (elapsed_nanos * speed) as i64;
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
                let cur_mouse: f64 = {
                    // we go 2ms in past to extrapolate less
                    let dt_secs: f64 = ((now.0 - latest_mouse_t.0).max(2_000_000) - 2_000_000)
                        as f64
                        / 1_000_000_000.;

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

    fn handle_event(&mut self, event: PlatterEvent) {
        let now = Instant::now();

        match event {
            PlatterEvent::MovePlayhead(jump) => {
                let cur_playhead = self.platter.get_playhead().record_pos;
                let new_pos = INanos(match jump {
                    Jump::ToZero => 0,
                    Jump::Forward => cur_playhead.0 + FF_TIME.0 as i64,
                    Jump::Backward => cur_playhead.0 - FF_TIME.0 as i64,
                });
                self.platter
                    .update_playhead(new_pos, self.platter.timestamp(now));
            }
            PlatterEvent::Nudge(direction) => match direction {
                Direction::Forward => self.nudges.forward.push_back(now),
                Direction::Backward => self.nudges.backward.push_back(now),
            },
        }
    }

    /// Updates virtual platter according to current state
    pub fn update_platter(&mut self) {
        // to not get stuck handling updates
        for _ in 0..1000 {
            if let Ok(upd) = self.events.try_recv() {
                self.handle_event(upd);
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
