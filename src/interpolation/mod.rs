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

impl Linear {
    pub fn interpolate_two(x0: u64, y0: f64, x1: u64, y1: f64, default_k: f64, x: u64) -> f64 {
        let k = if x0 == x1 {
            default_k
        } else {
            (y1 - y0) / (x1 - x0) as f64
        };

        y0 + (x as f64 - x0 as f64) * k
    }
}
