use std::time::{Duration, Instant};

use rtrb::Producer;

use crate::{
    scratch::record::Record,
    scratchv2::virtual_platter::{self, PlatterError, PlatterSample, VirtualPlatter},
};

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor<R> {
    extra_lattency: Duration,
    platter: VirtualPlatter,
    sample_rate: usize,
    record: R,
    buffer_size: usize,
}

impl<R: Record> PlatterAudioProcessor<R> {
    fn block_dur(buffer_size: usize, sample_rate: usize) -> Duration {
        Duration::from_secs_f64((buffer_size as f64 / 2.) / (sample_rate as f64))
    }

    /// Returns duration of the audio block requested by cpal (according to provided the buffer size and sample rate)
    pub fn block_duration(&self) -> Duration {
        Self::block_dur(self.buffer_size, self.sample_rate)
    }

    pub fn new(
        record: R,
        sample_rate: usize,
        buffer_size: usize,
        extra_lattency: Duration,
        platter_update_freq_hz: f64,
        jitter_factor: f64,
    ) -> (Self, Producer<PlatterSample>) {
        let block_duration = Self::block_dur(buffer_size, sample_rate);
        let (prod, platter) =
            VirtualPlatter::new(block_duration, platter_update_freq_hz, jitter_factor);
        let processor = PlatterAudioProcessor {
            extra_lattency,
            platter,
            sample_rate,
            record,
            buffer_size,
        };
        (processor, prod)
    }

    /// Converts position in seconds to sample number
    pub fn secs_to_sample(&self, secs: f64) -> f64 {
        self.sample_rate as f64 * secs
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, data: &mut [f32]) {
        let samples_n = data.len() as f64 / 2.;

        let block_duration = Duration::from_secs_f64(samples_n / self.sample_rate as f64);

        let finish = Instant::now() - self.extra_lattency;
        let start = finish - block_duration - self.extra_lattency;

        let handle_platter_error = |e: PlatterError, target_time: std::time::Instant| {
            println!("virtual platter error: {e:?}");
            match e {
                PlatterError::OldSampleRequested { oldest } => (oldest.clone(), oldest),
                PlatterError::NewerSampleRequested { newest } => (newest.clone(), newest),
                PlatterError::NoSamples => {
                    let default_sample = PlatterSample {
                        time: target_time,
                        record_pos: 0,
                    };
                    (default_sample.clone(), default_sample)
                }
            }
        };
        let start_pos = match self.platter.get_sample(start) {
            Ok(s) => s,
            Err(e) => handle_platter_error(e, start),
        };
        let finish_pos = match self.platter.get_sample(finish) {
            Ok(s) => s,
            Err(e) => handle_platter_error(e, finish),
        };

        let start_pos_secs = virtual_platter::interpolate(start_pos.0, start_pos.1, start);
        let finish_pos_secs = virtual_platter::interpolate(finish_pos.0, finish_pos.1, finish);

        let start_sample = self.secs_to_sample(start_pos_secs);
        let finish_sample = self.secs_to_sample(finish_pos_secs);
        println!("playing [{start_sample:.1}..{finish_sample:.1})");
        let step = (finish_sample - start_sample) / samples_n;
        let mut sample_i = start_sample;

        for frame in data.chunks_mut(2) {
            let sample = self.record.get_sample(sample_i);
            frame[0] = sample.l;
            frame[1] = sample.r;
            sample_i += step;
        }
    }
}
