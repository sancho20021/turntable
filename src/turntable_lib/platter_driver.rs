use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam::channel::Receiver;

use crate::{
    deck_controller::{DeckState, PlatterState},
    filters::exponential_decay_factor,
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

/// Most input events one platter update will absorb.
const MAX_EVENTS_PER_UPDATE: usize = 1000;

/// Furthest a nudge will bend the pitch, as a fraction of nominal speed.
const MAX_BEND: f64 = 0.16;

/// Platter updates per audio callback. One is the floor - below it a callback
/// finds no fresh sample and extrapolates from a stale slope - and the loop
/// below undershoots whatever it asks for, so two.
const UPDATES_PER_CALLBACK: f64 = 2.;

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
    /// pitch bend: input units moved since the last report, + being forward
    Nudge(i16),
}

/// Speed of a bend gesture, estimated from the nudge events it emits.
///
/// [`Self::ticks`] is a leaky sum: each event adds its input units, and the sum
/// decays with `tau_secs`. A steady `f` units per second settles it at
/// `f * tau_secs`, so dividing by `tau_secs` recovers `f` whatever the time
/// constant is - it shapes the attack and the release without touching how
/// deep a held bend goes.
struct NudgeVelocity {
    ticks: f64,
    tau_secs: f64,
    /// pitch bend per input unit per second
    responsiveness: f64,
    decayed_at: Instant,
}

impl NudgeVelocity {
    fn new(input: &InputProfile) -> Self {
        Self {
            ticks: 0.,
            tau_secs: input.nudge_release_tau_secs,
            responsiveness: input.nudge_responsiveness,
            decayed_at: Instant::now(),
        }
    }

    fn decay_to(&mut self, now: Instant) {
        let dt = now.duration_since(self.decayed_at).as_secs_f64();
        self.ticks *= exponential_decay_factor(dt, self.tau_secs);
        self.decayed_at = now;
    }

    fn push(&mut self, ticks: i16, now: Instant) {
        self.decay_to(now);
        self.ticks += f64::from(ticks);
    }

    /// Pitch offset, 0.01 being one percent fast.
    fn bend(&mut self, now: Instant) -> f64 {
        self.decay_to(now);
        (self.ticks / self.tau_secs * self.responsiveness).clamp(-MAX_BEND, MAX_BEND)
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
    nudges: NudgeVelocity,
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
        shutdown: Arc<AtomicBool>,
        buffer_frames_n: usize,
    ) -> Self {
        let record_speed = Speed::new(inertia_tau_secs, 0.005);
        let frequency_hz = Self::platter_update_freq(buffer_frames_n);
        log::info!("platter update frequency is a nominal {frequency_hz}hz");

        Self {
            deck_id,
            state,
            record_speed,
            nudges: NudgeVelocity::new(&input),
            input,
            platter,
            events,
            tracer: TelemetryTrace::new(),
            shutdown,
            frequency_hz,
        }
    }

    /// Nominal update frequency. The loop achieves somewhat less.
    fn platter_update_freq(buffer_frames_n: usize) -> usize {
        let callback_rate =
            1. / PlatterAudioProcessor::frames_to_dur(buffer_frames_n).as_secs_f64();
        (callback_rate * UPDATES_PER_CALLBACK) as usize
    }

    /// Calculates platter position in nanos
    fn calculate_position(&mut self) -> PlatterSample {
        let state = self.state.platter.load();
        let now = self.platter.now();

        let cur_playhead = self.platter.get_playhead();
        let elapsed_nanos: f64 = (now.0 as f64 - cur_playhead.timestamp_nanos.0 as f64).max(0.);

        // The pitch fader drives a motor and picks up its inertia; a hand on
        // the wheel does not, so the bend lands past the filter.
        let speed = self
            .record_speed
            .advance(elapsed_nanos / 1_000_000_000., self.state.target_speed())
            + self.nudges.bend(Instant::now());

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
            PlatterEvent::Nudge(ticks) => self.nudges.push(ticks, now),
        }
    }

    /// Updates virtual platter according to current state
    pub fn update_platter(&mut self) {
        // Bounded so a flood of input cannot starve the position update below.
        for _ in 0..MAX_EVENTS_PER_UPDATE {
            match self.events.try_recv() {
                Ok(event) => self.handle_event(event),
                Err(_) => break,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Ticks a second an FLX4 jog reports when turned at 33 1/3 rpm.
    const RECORD_SPEED_TICKS_PER_SEC: f64 = 400.;

    /// Bend after turning the jog at record speed for `secs`.
    fn turn_at_record_speed(tau_secs: f64, secs: f64) -> (NudgeVelocity, Instant, f64) {
        let mut nudges = NudgeVelocity {
            tau_secs,
            ..NudgeVelocity::new(&InputProfile::jog_wheel(1., 1.))
        };
        let start = nudges.decayed_at;
        let step = Duration::from_secs_f64(1. / RECORD_SPEED_TICKS_PER_SEC);
        let ticks = (secs * RECORD_SPEED_TICKS_PER_SEC) as u32;
        for i in 0..ticks {
            nudges.push(1, start + step * i);
        }
        let end = start + step * ticks;
        let bend = nudges.bend(end);
        (nudges, end, bend)
    }

    /// A bend held at a steady speed settles at the same depth for any tau.
    /// The shortfall is the sum sitting in the trough between two ticks, worth
    /// `0.5 / tau` ticks a second.
    #[test]
    fn a_held_bend_ignores_the_release_tau() {
        for tau in [0.02, 0.05, 0.2] {
            let (_, _, bend) = turn_at_record_speed(tau, 1.);
            assert!((bend - 0.10).abs() < 0.01, "tau {tau} held {bend}");
        }
    }

    /// A flick shorter than tau reaches part of the depth, less of it the
    /// longer tau is.
    #[test]
    fn a_flick_shorter_than_tau_bends_less() {
        let (_, _, brief) = turn_at_record_speed(0.1, 0.03);
        let (_, _, tight) = turn_at_record_speed(0.03, 0.03);
        assert!(brief < 0.04, "slow tau flicked to {brief}");
        assert!(tight > 0.06, "tight tau only flicked to {tight}");
    }

    #[test]
    fn the_bend_falls_away_over_one_tau() {
        let tau = 0.03;
        let (mut nudges, end, held) = turn_at_record_speed(tau, 1.);
        let after = |taus: f64| end + Duration::from_secs_f64(tau * taus);
        assert!((nudges.bend(after(1.)) / held - 0.368).abs() < 0.01);
        assert!(nudges.bend(after(3.)) / held < 0.06);
    }

    /// Forward and backward share one sum, so a reversal cancels within a few
    /// ticks.
    #[test]
    fn a_reversal_cancels_the_bend() {
        let (mut nudges, end, held) = turn_at_record_speed(0.03, 1.);
        assert!(held > 0.);
        for i in 0..12 {
            nudges.push(-1, end + Duration::from_secs_f64(i as f64 / 400.));
        }
        assert!(nudges.bend(end) < held / 2., "reversal left {}", nudges.bend(end));
    }
}
