//! Per-device tuning of the scratch control path.
//!
//! Every supported scratch input is reduced to the same abstract signal: an
//! absolute, monotonically-tracked **input position** measured in *input
//! units*. What one unit physically is depends on the device:
//!
//! * touchpad / mouse — one screen pixel of horizontal travel;
//! * jog wheel (MIDI) — one encoder tick, accumulated since startup.
//!
//! Nothing downstream of [`crate::deck_event::Event::ScratchMove`] knows which
//! of those it is dealing with, so the constants that genuinely differ between
//! devices are collected here instead of being hardcoded where they are used.
//! Two devices with the same profile behave identically.

/// Record time travelled per touchpad pixel at `sensitivity = 1.0`, in
/// nanoseconds. Chosen so that dragging across a 600 px window scratches
/// through roughly 0.9 s of audio.
const TOUCHPAD_BASE_SENSITIVITY: f64 = 1_500_000.0;

/// Tuning constants of one scratch input device.
///
/// All of these are read on the platter thread every update (via
/// [`crate::platter_driver::PlatterDriver`]) except
/// [`Self::speed_smoothing_tau_secs`], which is consumed once when the
/// controller's speed filter is built.
#[derive(Debug, Clone, Copy)]
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
}

impl InputProfile {
    /// Profile for the touchpad / mouse: input units are screen pixels, and
    /// events arrive at roughly the pointer's report rate (~125-1000 Hz).
    ///
    /// `sensitivity` is the user-facing multiplier on top of
    /// [`TOUCHPAD_BASE_SENSITIVITY`] (1.0 = default feel).
    pub fn touchpad(sensitivity: f64) -> Self {
        Self {
            nanos_per_input_unit: sensitivity * TOUCHPAD_BASE_SENSITIVITY,
            max_drift_units: 50,
            convergence_lambda: 50.0,
            speed_smoothing_tau_secs: 0.01,
        }
    }
}
