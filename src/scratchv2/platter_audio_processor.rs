use std::time::Duration;

use crate::{
    record::Record,
    scratchv2::virtual_platter::{PlatterSample, VirtualPlatter},
};

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor<R> {
    platter: VirtualPlatter,
    sample_rate: usize,
    record: R,
    buffer_size: usize,
    /// Last observed playhead position
    last_measurement: PlatterSample,
    /// last estimated moving average velocity in nanosec of song / nanosec
    last_ema_vel: f64,
    /// Index of the next sample to play
    cur_playhead: f64,
}

impl<R: Record> PlatterAudioProcessor<R> {
    fn block_dur(buffer_size: usize, sample_rate: usize) -> Duration {
        Duration::from_secs_f64((buffer_size as f64 / 2.) / (sample_rate as f64))
    }

    /// Returns current estimated moving average velocity in nanosec of song / nanosec
    fn cur_ema_vel(&self, cur_measurement: PlatterSample) -> f64 {
        let t_prev = self.last_measurement.timestamp_nanos;
        let t_now = cur_measurement.timestamp_nanos;

        // This is a corner case that shouldn't normally happen. New timestamp is behind old timestamp, so we just return current velocity
        // TODO: well looks like it happens when updates from virtual platter are slow
        if t_now <= t_prev {
            return self.last_ema_vel;
        }

        let dt = t_now - t_prev;
        // let alpha = 0.3;  0.3 is good!
        let alpha = 0.25;
        let cur_vel =
            (cur_measurement.record_pos - self.last_measurement.record_pos) as f64 / dt as f64;

        (alpha * cur_vel) + ((1. - alpha) * self.last_ema_vel)
    }

    pub fn new(record: R, sample_rate: usize, buffer_size: usize) -> (Self, VirtualPlatter) {
        let platter = VirtualPlatter::new();
        let last_measurement = platter.get_playhead();
        let processor = PlatterAudioProcessor {
            platter: platter.clone(),
            sample_rate,
            record,
            buffer_size,
            last_measurement,
            last_ema_vel: 0.,
            cur_playhead: 0.,
        };
        (processor, platter)
    }

    /// Converts position in nanoseconds to sample number
    pub fn nanosecs_to_sample(&self, nanos: f64) -> f64 {
        self.sample_rate as f64 * (nanos / 1_000_000_000.)
    }

    /// Converts sample number to nanosecs
    pub fn sample_to_nanos(&self, sample: f64) -> f64 {
        sample / self.sample_rate as f64 * 1_000_000_000.
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, data: &mut [f32]) {
        let samples_n = data.len() as f64 / 2.;

        let cur_measurement = self.platter.get_playhead();
        let cur_vel = self.cur_ema_vel(cur_measurement);

        // println!(
        //     "{{ timestamp: {}, virtual_playhead: {}, real_playhead: {}, speed: {:.4}}}",
        //     cur_measurement.timestamp_nanos,
        //     cur_measurement.record_pos,
        //     self.sample_to_nanos(self.cur_playhead).round() as i64,
        //     cur_vel
        // );

        let block_duration = Duration::from_secs_f64(samples_n / self.sample_rate as f64);

        let mut playhead = self.cur_playhead;
        let samples_to_play = {
            let nanos_to_play = block_duration.as_nanos() as f64 * cur_vel;
            self.nanosecs_to_sample(nanos_to_play)
        };
        let step = samples_to_play / samples_n;

        // println!("playing [{playhead:.0}..{:.0})", playhead + samples_to_play);

        for frame in data.chunks_mut(2) {
            let sample = self.record.get_sample(playhead);
            frame[0] = sample.l;
            frame[1] = sample.r;
            playhead += step;
        }

        self.cur_playhead = playhead;
        self.last_measurement = cur_measurement;
        self.last_ema_vel = cur_vel;
    }
}
