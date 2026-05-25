use crate::stereo_frame::StereoFrame;

/// Pure DSP interpolation.
///
/// Responsibilities:
/// - sample reconstruction
pub trait Interpolator: Send + 'static {
    /// Must be very fast. Can't do any allocation
    fn interpolate(&self, samples: &[StereoFrame], position: f64) -> StereoFrame;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Linear;

impl Interpolator for Linear {
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
}
