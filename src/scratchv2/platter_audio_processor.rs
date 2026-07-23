use std::time::Duration;

use crate::{
    interpolation::Linear,
    record::Record,
    scratchv2::virtual_platter::{INanos, PlatterSample, UNanos, VirtualPlatter},
};

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor<R> {
    platter: VirtualPlatter,
    sample_rate: usize,
    record: R,
    /// timestamp and nanosecond of last sample played
    last_played: PlatterSample,
    /// Last observed virtual playhead position
    last_measurement: PlatterSample,
}

impl<R: Record> PlatterAudioProcessor<R> {
    fn block_dur(&self, buffer_size: usize) -> Duration {
        Duration::from_secs_f64((buffer_size as f64 / 2.) / (self.sample_rate as f64))
    }

    pub fn new(record: R, sample_rate: usize) -> (Self, VirtualPlatter) {
        let platter = VirtualPlatter::new();
        let last_measurement = platter.get_playhead();
        let processor = PlatterAudioProcessor {
            platter: platter.clone(),
            sample_rate,
            record,
            last_played: last_measurement,
            last_measurement,
        };
        (processor, platter)
    }

    /// Converts position in nanoseconds to sample number
    pub fn nanosecs_to_sample(&self, nanos: INanos) -> f64 {
        self.sample_rate as f64 * (nanos.0 as f64 / 1_000_000_000.)
    }

    /// Converts sample number to nanosecs
    pub fn sample_to_nanos(&self, sample: f64) -> f64 {
        sample / self.sample_rate as f64 * 1_000_000_000.
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, data: &mut [f32]) {
        let samples_n = data.len() as f64 / 2.;

        let cur_measurement = self.platter.get_playhead();
        let block_duration = self.block_dur(data.len());

        let target_timestamp =
            UNanos(self.last_played.timestamp_nanos.0 + block_duration.as_nanos() as u64);
        // todo: with time, play_until may lag behind or rush in front of current virtual platter measurement.
        // in this case, we should potentially increase or decrease block_duration dynamically so that
        // the audio processor clock synchronizes with virtual platter clock.

        let target_playhead_nanos = INanos(Linear::interpolate_two(
            self.last_measurement.timestamp_nanos.0,
            self.last_measurement.record_pos.0 as f64,
            cur_measurement.timestamp_nanos.0,
            cur_measurement.record_pos.0 as f64,
            1., // if interpolation can't be done, assume normal playback speed = 1
            target_timestamp.0,
        ) as i64);

        let mut playhead = self.nanosecs_to_sample(self.last_played.record_pos);
        let target_playhead = self.nanosecs_to_sample(target_playhead_nanos);
        let step = (target_playhead - playhead) / samples_n;

        log::debug!(
            "observations at {}, {}",
            self.last_measurement.timestamp_nanos.0,
            cur_measurement.timestamp_nanos.0
        );
        log::debug!("playing at = {:.0}, {:.0}", self.last_played.timestamp_nanos.0, target_timestamp.0);

        log::debug!(
            "playing [{playhead:.0}..{:.0}) at speed {:.2}",
            target_playhead,
            step
        );

        for frame in data.chunks_mut(2) {
            let sample = self.record.get_sample(playhead);
            frame[0] = sample.l;
            frame[1] = sample.r;
            playhead += step;
        }

        self.last_played = PlatterSample {
            timestamp_nanos: target_timestamp,
            record_pos: target_playhead_nanos,
        };
        self.last_measurement = cur_measurement;
    }
}
