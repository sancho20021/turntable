//! Per-device tuning of the scratch control path.
//!
//! Every supported scratch input is reduced to the same abstract signal: an
//! absolute, monotonically-tracked **input position** measured in *input
//! units*. What one unit physically is depends on the device:
//!
//! * touchpad / mouse — one screen pixel of horizontal travel;
//! * jog wheel (MIDI) — one encoder tick, accumulated since startup.
//!
//! Nothing downstream of [`crate::input_event::DeckCommand::ScratchMove`] knows which
//! of those it is dealing with, so the constants that genuinely differ between
//! devices are collected here instead of being hardcoded where they are used.
//! Two devices with the same profile behave identically.

use crate::midi::flx4::JOG_TICKS_PER_REVOLUTION;

/// Record time travelled per touchpad pixel at `sensitivity = 1.0`, in
/// nanoseconds. Chosen so that dragging across a 600 px window scratches
/// through roughly 0.9 s of audio.
const TOUCHPAD_BASE_SENSITIVITY: f64 = 1_500_000.0;

/// One revolution of a record at 33 1/3 rpm, in nanoseconds (60 / 33.333 s).
const RECORD_REVOLUTION_NANOS: u64 = 1_800_000_000;

/// A brisk scroll flick, in detents a second.
const TOUCHPAD_FLICK_DETENTS_PER_SEC: f64 = 20.;

/// Pitch bend from a flick at [`TOUCHPAD_FLICK_DETENTS_PER_SEC`], at
/// `nudge = 1.0`.
const TOUCHPAD_BEND_AT_FLICK: f64 = 0.04;

/// Pitch bend from turning the jog at 33 1/3 rpm, at `nudge = 1.0`.
const JOG_BEND_AT_RECORD_SPEED: f64 = 0.4;

/// Tuning constants of one scratch input device.
///
/// All of these are read on the platter thread every update (via
/// [`crate::platter_driver::PlatterDriver`]) except
/// [`Self::speed_smoothing_tau_secs`], which is consumed once when the
/// controller's speed filter is built.
#[derive(Debug, Clone)]
pub struct InputProfile {
    /// **Scratch gain**: nanoseconds of record time per one input unit of
    /// travel, i.e. how far the playhead moves for a given amount of input
    /// movement.
    ///
    /// Unit: nanoseconds / input unit. Higher = more audio per pixel/tick, so
    /// scratches sound faster and shorter movements cover more of the track.
    pub nanos_per_input_unit: f64,

    /// **Extrapolation limit**: how far ahead of the last reported position the
    /// predicted position is allowed to run.
    ///
    /// Unit: input units. Input arrives in discrete events, so between events
    /// the platter thread extrapolates from the last known position and speed;
    /// this caps the damage when the user stops moving right after a fast
    /// stroke (without it, a stale high speed keeps flinging the playhead
    /// forward). Lower = safer but more audible stepping on fast movement.
    pub max_drift_units: i64,

    /// **Convergence rate** at which the extrapolated position is blended back
    /// onto the last actually-reported position.
    ///
    /// Unit: 1 / seconds (exponential decay rate). Higher = snaps onto real
    /// input faster (tighter, but rougher between sparse events); lower =
    /// smoother, with more perceived inertia and latency. As a rule of thumb
    /// pick ~1 / (typical gap between input events).
    pub convergence_lambda: f64,

    /// **Speed smoothing** time constant of the low-pass filter that estimates
    /// input velocity from successive positions.
    ///
    /// Unit: seconds. The raw per-event velocity is very noisy because event
    /// timing jitters; this is how much of that noise is averaged out. Higher =
    /// steadier speed estimate but slower to react to direction changes.
    pub speed_smoothing_tau_secs: f64,

    /// **Nudge strength**: pitch bend per input unit per second of nudge
    /// movement, a bend of 0.01 being one percent fast.
    ///
    /// Unit: seconds per input unit.
    pub nudge_responsiveness: f64,

    /// **Nudge release**: time constant of the leaky sum that turns nudge
    /// events into a gesture speed (see [`crate::platter_driver`]).
    ///
    /// Unit: seconds. Sets how fast a bend builds and how fast it falls away
    /// once the hand stops: after one of these the bend is 63% of the way in,
    /// or 63% of the way out. Keep it well above the gap between nudge events,
    /// or the bend steps audibly - a scroll wheel wants far more of it than a
    /// jog reporting 400 ticks a second.
    pub nudge_release_tau_secs: f64,
}

impl InputProfile {
    /// Profile for the touchpad / mouse: input units are screen pixels, and
    /// events arrive at roughly the pointer's report rate (~125-1000 Hz).
    ///
    /// `sensitivity` is the user-facing multiplier on top of
    /// [`TOUCHPAD_BASE_SENSITIVITY`] (1.0 = default feel), and `nudge` the one
    /// on top of [`TOUCHPAD_BEND_AT_FLICK`].
    pub fn touchpad(sensitivity: f64, nudge: f64) -> Self {
        Self {
            nanos_per_input_unit: sensitivity * TOUCHPAD_BASE_SENSITIVITY,
            max_drift_units: 50,
            convergence_lambda: 50.0,
            speed_smoothing_tau_secs: 0.01,
            nudge_responsiveness: nudge * TOUCHPAD_BEND_AT_FLICK
                / TOUCHPAD_FLICK_DETENTS_PER_SEC,
            nudge_release_tau_secs: 0.1,
        }
    }

    /// Profile for a jog wheel: input units are encoder ticks, accumulated by
    /// [`crate::midi::flx4::Decoder`] into an absolute wheel position.
    ///
    /// The gain makes the wheel behave like the record it stands in for: one
    /// revolution covers [`RECORD_REVOLUTION_NANOS`] of audio, the same as a
    /// platter at 33 1/3 rpm, so a full turn of the wheel is a full turn of the
    /// record.
    ///
    /// The three filter constants are the touchpad's values as a starting
    /// point. Jog ticks arrive at a different rate and quantisation, so they
    /// want tuning against a `trace-input` capture rather than trust.
    pub fn jog_wheel(sensitivity: f64, nudge: f64) -> Self {
        let nanos_per_tick = RECORD_REVOLUTION_NANOS as f64 / JOG_TICKS_PER_REVOLUTION as f64;
        let ticks_per_sec_at_record_speed = 1e9 / nanos_per_tick;
        Self {
            nanos_per_input_unit: sensitivity * nanos_per_tick,
            max_drift_units: 20,
            convergence_lambda: 50.0,
            speed_smoothing_tau_secs: 0.01,
            nudge_responsiveness: nudge * JOG_BEND_AT_RECORD_SPEED
                / ticks_per_sec_at_record_speed,
            nudge_release_tau_secs: 0.03,
        }
    }
}
