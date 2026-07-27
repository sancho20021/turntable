//! Speed of a spinning motor slowly adjusting to the set speed.
//
// Used to emulate vinyl wind up and wind down effect, and delayed speed reaction to pitch adjust

#[derive(Debug)]
pub struct Speed {
    /// The inertia time constant (in seconds).
    /// A lower value (e.g., 0.2) simulates a high-torque direct-drive motor like a Technics SL-1200.
    /// A higher value (e.g., 1.0) simulates a low-torque belt-drive motor.
    inertia_tau: f64,
    /// Dynamically adjustable speed
    speed: f64,
}

impl Speed {
    pub fn new(inertia_tau: f64, initial_speed: f64) -> Self {
        Self {
            inertia_tau,
            speed: initial_speed,
        }
    }

    pub fn get(&self) -> f64 {
        self.speed
    }

    /// Update current motor speed according to delta time and target speed
    pub fn advance_speed(&mut self, dt_secs: f64, target_speed: f64) {
        // Avoid endless micro-calculations if the speeds are virtually identical
        if (self.speed - target_speed).abs() > 1e-6 {
            let factor = (-dt_secs / self.inertia_tau).exp();
            let next_speed = target_speed + (self.speed - target_speed) * factor;
            self.speed = next_speed;
        } else {
            self.speed = target_speed;
        }
    }

    /// sets motor speed
    pub fn hard_set_speed(&mut self, speed: f64) {
        self.speed = speed;
    }
}
