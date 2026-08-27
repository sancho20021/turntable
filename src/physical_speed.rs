//! Speed of a spinning motor slowly adjusting to the set speed.
//
// Used to emulate vinyl wind up and wind down effect, and delayed speed reaction to pitch adjust

use crate::filters::FirstOrderLPF;

pub struct Speed {
    speed: FirstOrderLPF,
    /// if difference between current and desired speed is below this value,
    /// the current speed is set to desired speed.
    ///
    /// Useful to prevent record from approaching target speed for too long
    diff_threshold: f64,
}

impl Speed {
    pub fn new(inertia_secs: f64, diff_threshold: f64) -> Self {
        Self {
            speed: FirstOrderLPF::new(inertia_secs),
            diff_threshold,
        }
    }

    /// Update current motor speed according to delta time and target speed
    pub fn advance(&mut self, dt_secs: f64, target_speed: f64) -> f64 {
        let speed = self.speed.advance(dt_secs, target_speed);
        // Avoid endless adjustments if the speeds are virtually identical

        if (speed - target_speed).abs() < self.diff_threshold {
            self.speed.force_state(target_speed);
            target_speed
        } else {
            speed
        }
    }
}
