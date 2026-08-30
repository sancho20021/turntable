/// Pure mathematical helper to compute the time-continuous exponential decay factor.
/// Returns a value between 0.0 (instant change) and 1.0 (no change).
#[inline]
pub fn exponential_decay_factor(dt_secs: f64, tau: f64) -> f64 {
    (-dt_secs / tau).exp()
}

/// Stateful First-Order Low-Pass Filter
/// Used for Playhead position filtering and Motor Speed tracking.
pub struct FirstOrderLPF {
    state: Option<f64>,
    pub tau: f64,
}

impl FirstOrderLPF {
    pub fn new(tau_secs: f64) -> Self {
        Self {
            state: None,
            tau: tau_secs,
        }
    }

    pub fn reset(&mut self) {
        self.state = None;
    }

    /// Advances the filter with a new raw target value.
    pub fn advance(&mut self, dt_secs: f64, raw_target: f64) -> f64 {
        let factor = exponential_decay_factor(dt_secs, self.tau);

        let current_state = match self.state {
            Some(s) => s,
            None => {
                self.state = Some(raw_target);
                return raw_target; // Initialize instantly on first frame
            }
        };

        // Equivalent to standard ma_filter: (alpha * raw) + ((1-alpha) * state)
        let next_state = raw_target + (current_state - raw_target) * factor;
        self.state = Some(next_state);
        next_state
    }

    /// Overwrite the internal memory directly (useful for JUMPS).
    pub fn force_state(&mut self, forced_value: f64) {
        self.state = Some(forced_value);
    }
}
