use crate::{record::interpolation::Interpolator, stereo_frame::StereoFrame};

#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub struct UNanos(pub u64);

impl UNanos {
    pub fn as_millis(&self) -> u64 {
        self.0 / 1_000_000
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub struct INanos(pub i64);

impl INanos {
    pub fn as_millis(&self) -> i64 {
        self.0 / 1_000_000
    }
}

/// Virtual record
#[derive(Debug)]
pub struct Record {
    interpolator: Interpolator,
    samples: Vec<StereoFrame>,
    sample_rate: usize,
}

impl Record {
    pub fn new(samples: Vec<StereoFrame>, interpolator: Interpolator, sample_rate: usize) -> Self {
        Self {
            samples,
            interpolator,
            sample_rate,
        }
    }

    /// Converts position in nanoseconds to sample number
    pub fn nanosecs_to_sample(&self, nanos: INanos) -> f64 {
        self.sample_rate as f64 * (nanos.0 as f64 / 1_000_000_000.)
    }
    pub fn get_sample(&self, position: INanos) -> StereoFrame {
        let position = self.nanosecs_to_sample(position);

        if !(0. <= position && position < self.samples.len() as f64) {
            return StereoFrame::default();
        }
        self.interpolator.interpolate(&self.samples, position)
    }
}

pub mod interpolation {
    use crate::stereo_frame::StereoFrame;

    #[derive(Debug)]
    pub enum Interpolator {
        Linear(Linear),
    }

    impl Interpolator {
        pub fn linear() -> Self {
            Self::Linear(Linear)
        }

        pub fn interpolate(&self, samples: &[StereoFrame], position: f64) -> StereoFrame {
            match self {
                Interpolator::Linear(linear) => linear.interpolate(samples, position),
            }
        }
    }

    #[derive(Debug)]
    pub struct Linear;

    impl Linear {
        fn interpolate(&self, samples: &[StereoFrame], position: f64) -> StereoFrame {
            if samples.len() < 2 {
                return StereoFrame::default();
            }

            // before start
            if position < 0.0 {
                return StereoFrame::default();
            }

            let base = position.floor() as usize;

            // after end
            if base + 1 >= samples.len() {
                return StereoFrame::default();
            }

            let frac = position.fract() as f32;

            let a = samples[base];
            let b = samples[base + 1];

            StereoFrame {
                l: a.l + (b.l - a.l) * frac,
                r: a.r + (b.r - a.r) * frac,
            }
        }

        pub fn interpolate_two(x0: u64, y0: f64, x1: u64, y1: f64, default_k: f64, x: u64) -> f64 {
            let k = if x0 == x1 {
                default_k
            } else {
                (y1 - y0) / (x1 - x0) as f64
            };

            y0 + (x as f64 - x0 as f64) * k
        }
    }
}
