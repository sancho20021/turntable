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

const TOUCHPAD_NUDGE_MULTIPLIER: f32 = 2.0;

/// Nudge strength per jog-wheel bend message, at `nudge_responsiveness = 1.0`.
/// Turning the side of the wheel emits a nudge per encoder tick, i.e. many per
/// gesture rather than one per press, so each one has to count for much less.
const JOG_NUDGE_MULTIPLIER: f32 = 0.25;

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

    /// **Nudge strength**: how much pitch bend a single nudge event is worth
    /// while it is alive (see [`crate::platter_driver`]).
    ///
    /// Unit: percent of nominal speed per in-flight nudge. Devices differ in
    /// how many nudge events one gesture produces, so the user-facing
    /// `nudge_responsiveness` is scaled per device to keep a nudge feeling the
    /// same regardless of what emitted it.
    pub nudge_responsiveness: f32,
}

impl InputProfile {
    /// Profile for the touchpad / mouse: input units are screen pixels, and
    /// events arrive at roughly the pointer's report rate (~125-1000 Hz).
    ///
    /// `sensitivity` is the user-facing multiplier on top of
    /// [`TOUCHPAD_BASE_SENSITIVITY`] (1.0 = default feel), and
    /// `nudge_responsiveness` the one on top of
    /// [`TOUCHPAD_NUDGE_MULTIPLIER`].
    pub fn touchpad(sensitivity: f64, nudge_responsiveness: f32) -> Self {
        Self {
            nanos_per_input_unit: sensitivity * TOUCHPAD_BASE_SENSITIVITY,
            max_drift_units: 50,
            convergence_lambda: 50.0,
            speed_smoothing_tau_secs: 0.01,
            nudge_responsiveness: nudge_responsiveness * TOUCHPAD_NUDGE_MULTIPLIER,
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
    /// The remaining three are the touchpad's values as a starting point. Jog
    /// ticks arrive at a different rate and quantisation, so they want tuning
    /// against a `trace-input` capture rather than trust.
    pub fn jog_wheel(sensitivity: f64, nudge_responsiveness: f32) -> Self {
        let nanos_per_tick = RECORD_REVOLUTION_NANOS as f64 / JOG_TICKS_PER_REVOLUTION as f64;
        Self {
            nanos_per_input_unit: sensitivity * nanos_per_tick,
            max_drift_units: 20,
            convergence_lambda: 50.0,
            speed_smoothing_tau_secs: 0.01,
            nudge_responsiveness: nudge_responsiveness * JOG_NUDGE_MULTIPLIER,
        }
    }
}
