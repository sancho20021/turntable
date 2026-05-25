use crate::{interpolation::Interpolator, stereo_frame::StereoFrame};

/// Interface of a virtual record
pub trait Record {
    /// Warning: this function must be very fast. Can't do any allocation
    fn get_sample(&self, position: f64) -> StereoFrame;
}

#[derive(Debug)]
pub struct InterpolatedRecord<Int> {
    interpolator: Int,
    samples: Vec<StereoFrame>,
}

impl<Int> InterpolatedRecord<Int> {
    pub fn new(samples: Vec<StereoFrame>, interpolator: Int) -> Self {
        Self {
            samples,
            interpolator,
        }
    }
}

impl<Int: Interpolator> Record for InterpolatedRecord<Int> {
    fn get_sample(&self, position: f64) -> StereoFrame {
        if !(0. <= position && position < self.samples.len() as f64) {
            return StereoFrame::default();
        }
        self.interpolator.interpolate(&self.samples, position)
    }
}
