use crate::{deck::interpolator::Interpolator, stereo_frame::StereoFrame};

#[derive(Debug)]
pub struct Record<Int> {
    interpolator: Int,
    samples: Vec<StereoFrame>,
}

impl<Int: Interpolator> Record<Int> {
    pub fn new(samples: Vec<StereoFrame>, interpolator: Int) -> Self {
        Self {
            samples,
            interpolator,
        }
    }

    /// Warning: this function must be very fast. Can't do any allocation
    pub fn get_sample(&self, position: f64) -> StereoFrame {
        if !(0. <= position && position < self.samples.len() as f64) {
            return StereoFrame::default();
        }
        self.interpolator.interpolate(&self.samples, position)
    }
}
