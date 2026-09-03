use crate::stereo_frame::StereoFrame;

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

/// One-pole DC blocker: `y[n] = x[n] - x[n-1] + r * y[n-1]`, with the pole at
/// `r = exp(-2*pi*fc/fs)`.
/// (High Pass Filter)
pub struct DcBlocker {
    /// pole radius, `exp(-2*pi*fc/fs)`
    r: f64,
    /// previous input, the differentiator's memory
    last_input: f64,
    /// previous output, the pole's memory
    state: f64,
}

impl DcBlocker {
    pub fn new(corner_hz: f64, sample_rate: u32) -> Self {
        Self {
            r: (-std::f64::consts::TAU * corner_hz / sample_rate as f64).exp(),
            last_input: 0.,
            state: 0.,
        }
    }

    /// Passes samples through untouched: at `r == 1` the difference and the pole
    /// telescope exactly. Lets a test read the playhead off the raw samples.
    pub fn bypass() -> Self {
        Self {
            r: 1.,
            last_input: 0.,
            state: 0.,
        }
    }

    /// Time constant of the decay, `1 / (2*pi*fc)`. The offset is 60dB down
    /// after about seven of these.
    pub fn tau_secs(corner_hz: f64) -> f64 {
        1. / (std::f64::consts::TAU * corner_hz)
    }

    #[inline]
    pub fn advance(&mut self, input: f64) -> f64 {
        // f64 state throughout: the pole sits within 2e-3 of the unit circle, so
        // f32 rounding of the feedback term is a sizeable fraction of the
        // distance the filter has left to travel.
        let output = input - self.last_input + self.r * self.state;
        self.last_input = input;
        self.state = output;
        output
    }
}

/// A [`DcBlocker`] per channel, since one shared state would sum the two.
pub struct StereoDcBlocker {
    l: DcBlocker,
    r: DcBlocker,
}

impl StereoDcBlocker {
    pub fn new(corner_hz: f64, sample_rate: u32) -> Self {
        Self {
            l: DcBlocker::new(corner_hz, sample_rate),
            r: DcBlocker::new(corner_hz, sample_rate),
        }
    }

    pub fn bypass() -> Self {
        Self {
            l: DcBlocker::bypass(),
            r: DcBlocker::bypass(),
        }
    }

    #[inline]
    pub fn advance(&mut self, frame: StereoFrame) -> StereoFrame {
        StereoFrame {
            l: self.l.advance(frame.l as f64) as f32,
            r: self.r.advance(frame.r as f64) as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 48_000;
    const CORNER_HZ: f64 = 15.;

    fn peak_of_sine(hz: f64, secs: f64, blocker: &mut DcBlocker) -> f64 {
        let n = (secs * FS as f64) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / FS as f64;
                blocker
                    .advance((std::f64::consts::TAU * hz * t).sin())
                    .abs()
            })
            // the tail of the run, once the filter has settled
            .skip(n / 2)
            .fold(0., f64::max)
    }

    /// The whole point: a held level must not survive.
    #[test]
    fn a_held_level_drains_away() {
        let mut blocker = DcBlocker::new(CORNER_HZ, FS);
        let mut out = 0.;
        // seven time constants, the figure quoted at the call site
        for _ in 0..(7. * DcBlocker::tau_secs(CORNER_HZ) * FS as f64) as usize {
            out = blocker.advance(0.5);
        }
        assert!(out.abs() < 0.5 / 1000., "still holding {out} of 0.5");
    }

    /// It must cost the music nothing anyone can hear.
    #[test]
    fn the_passband_is_left_alone() {
        for hz in [50., 100., 1_000.] {
            let mut blocker = DcBlocker::new(CORNER_HZ, FS);
            let peak = peak_of_sine(hz, 0.5, &mut blocker);
            let db = 20. * peak.log10();
            assert!(db > -0.5, "{hz}Hz down {db:.2}dB");
        }
    }

    /// Below the corner it rolls off like the cartridge it stands in for.
    #[test]
    fn subsonics_roll_off() {
        let mut blocker = DcBlocker::new(CORNER_HZ, FS);
        let db = 20. * peak_of_sine(2., 4., &mut blocker).log10();
        assert!(db < -12., "2Hz only down {db:.2}dB");
    }

    /// A bypassed blocker has to be exactly transparent, since tests read
    /// playhead positions straight off the samples it passes.
    #[test]
    fn bypass_is_exact() {
        let mut blocker = DcBlocker::bypass();
        for x in [0., 0.5, -0.25, 1., 1., -1., 0.125] {
            assert_eq!(blocker.advance(x), x);
        }
    }
}
