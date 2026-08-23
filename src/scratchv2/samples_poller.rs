use crate::{scratchv2::platter_audio_processor::PlatterAudioProcessor, stereo_frame::StereoFrame};

pub struct SamplesPoller<const CHANNELS: usize> {
    /// samples buffers
    buffers: [Vec<StereoFrame>; CHANNELS],
    audio_processors: [PlatterAudioProcessor; CHANNELS],
}

impl<const CHANNELS: usize> SamplesPoller<CHANNELS> {
    pub fn new(samples_n: usize, audio_processors: [PlatterAudioProcessor; CHANNELS]) -> Self {
        let buf = vec![StereoFrame::default(); samples_n];
        Self {
            buffers: std::array::from_fn(|_| buf.clone()),
            audio_processors,
        }
    }
}

// todo: implement for arbitrary number of channels
impl SamplesPoller<1> {
    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, data: &mut [f32]) {
        let total_samples = data.len() / 2;
        if total_samples > self.buffers[0].len() {
            // todo: do not panic here, go to default silent mode and send signal to controller to stop everything
            panic!(
                "actual buffer length in samples ({}) is bigger than configured ({})",
                total_samples,
                self.buffers[0].len()
            );
        }
        self.audio_processors[0].write_frames(self.buffers[0].as_mut_slice());

        for (i, frame) in data.chunks_mut(2).enumerate() {
            frame[0] = self.buffers[0][i].l;
            frame[1] = self.buffers[0][i].r;
        }
    }
}
