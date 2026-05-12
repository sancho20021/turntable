use crate::deck::interpolator::{self, Interpolator};
use crate::deck::transport::Transport;
use crate::stereo_frame::StereoFrame;

pub struct AudioEngine<Int> {
    pub samples: Vec<StereoFrame>,
    pub transport: Transport,
    pub playing: bool,
    pub interpolator: Int,
}

impl<Int: Interpolator> AudioEngine<Int> {
    pub fn new(samples: Vec<StereoFrame>, interpolator: Int) -> Self {
        Self {
            samples,
            transport: Transport::new(),
            playing: true,
            interpolator,
        }
    }

    pub fn next_frame(&mut self) -> StereoFrame {
        if !self.playing {
            return StereoFrame::default();
        }

        let out = self
            .interpolator
            .interpolate(&self.samples, self.transport.position);

        self.transport.advance();

        // stop at end
        if self.transport.position >= self.samples.len() as f64 {
            self.playing = false;
        }

        out
    }
}
