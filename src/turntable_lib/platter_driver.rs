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
    deck_controller::{DeckState, PlatterState},
    input_event::Direction,
    input_profile::InputProfile,
    physical_speed::Speed,
    platter_audio_processor::PlatterAudioProcessor,
    record::{INanos, UNanos},
    record_input,
    telemetry::TelemetryTrace,
    virtual_platter::{PlatterSample, WritablePlatter},
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
    deck_id: usize,
    state: Arc<DeckState>,
    record_speed: Speed,
    /// tuning of the scratch input device currently driving this deck
    input: InputProfile,
    platter: WritablePlatter,
    events: Receiver<PlatterEvent>,
    nudges: NudgeQueue,
    /// for recording metrics
    pub tracer: TelemetryTrace,
    shutdown: Arc<AtomicBool>,
    frequency_hz: usize,
}

impl PlatterDriver {
    pub fn new(
        deck_id: usize,
        state: Arc<DeckState>,
        input: InputProfile,
        inertia_tau_secs: f64,
        platter: WritablePlatter,
        events: Receiver<PlatterEvent>,
        nudge_responsiveness: f32,
        shutdown: Arc<AtomicBool>,
        buffer_frames_n: usize,
    ) -> Self {
        let record_speed = Speed::new(inertia_tau_secs, 0.005);
        let frequency_hz = Self::platter_update_freq(buffer_frames_n);
        log::info!("calculated platter update frequency is {frequency_hz}hz");

        Self {
            deck_id,
            state,
            record_speed,
            input,
            platter,
            events,
            tracer: TelemetryTrace::new(),
            nudges: NudgeQueue::new(nudge_responsiveness),
            shutdown,
            frequency_hz,
        }
    }

    /// Calculates optimal update frequency
    fn platter_update_freq(buffer_frames_n: usize) -> usize {
        (1. / PlatterAudioProcessor::frames_to_dur(buffer_frames_n).as_secs_f64() * 3.) as usize
    }

    /// Calculates platter position in nanos
    fn calculate_position(&mut self) -> PlatterSample {
        let state = self.state.platter.load();
        let now = self.platter.now();

        let cur_playhead = self.platter.get_playhead();
        let elapsed_nanos: f64 = (now.0 as f64 - cur_playhead.timestamp_nanos.0 as f64).max(0.);

        let target_speed = {
            let nudge_raw = self.nudges.current_nudge() as f64;
            let nudge_modifier = nudge_raw.clamp(-16., 16.) / 100.;
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
                anchor_input,
                latest_input,
                timestamp: latest_input_t,
                input_speed,
            } => {
                let cur_input: f64 = {
                    // we go 2ms in past to extrapolate less
                    let dt_secs: f64 = ((now.0 - latest_input_t.0).max(2_000_000) - 2_000_000)
                        as f64
                        / 1_000_000_000.;

                    // 1. Calculate where the input *would* be if it kept moving
                    let extrapolated_input = {
                        // for extrapolation we clamp dt
                        let dt_secs = dt_secs.clamp(-20. / 1_000., 10. / 1_000.);

                        let raw_extrapolated: f64 = match input_speed {
                            Some(speed) => latest_input as f64 + (speed * dt_secs),
                            None => latest_input as f64,
                        };

                        // never run further than the device's profile allows
                        let max_drift = self.input.max_drift_units as f64;
                        raw_extrapolated.clamp(
                            latest_input as f64 - max_drift,
                            latest_input as f64 + max_drift,
                        )
                    };

                    record_input!(
                        self.tracer,
                        now,
                        format!("extrapolated_input_{}", self.deck_id),
                        extrapolated_input
                    );

                    // 2. Convergence factor (higher lambda = snaps faster, lower = smoother/more inertia)
                    let convergence_weight = (-self.input.convergence_lambda * dt_secs).exp(); // Drops from 1.0 to 0.0 over time

                    // 3. Blend between extrapolation (short-term) and the hard target (long-term)
                    (extrapolated_input * convergence_weight)
                        + (latest_input as f64 * (1.0 - convergence_weight))
                };

                record_input!(
                    self.tracer,
                    now,
                    format!("converged_input_{}", self.deck_id),
                    cur_input
                );

                let input_delta = cur_input - anchor_input as f64;

                // Map input movement to playhead offset
                let position_delta = (input_delta * self.input.nanos_per_input_unit) as i64;
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

    pub fn start(mut self) -> std::thread::JoinHandle<Self> {
        std::thread::spawn(move || {
            let interval = Duration::from_secs_f64(1.0 / self.frequency_hz as f64);

            while !self.shutdown.load(Ordering::Relaxed) {
                let loop_start = Instant::now();
                self.update_platter();
                // 5. High-precision sleep to maintain targeted update frequency
                let elapsed = loop_start.elapsed();
                if elapsed < interval {
                    std::thread::sleep(interval - elapsed);
                }
            }

            log::info!("Platter stopped");
            self
        })
    }
}
