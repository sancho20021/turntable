use std::time::Duration;

use crate::{
    interpolation::Linear,
    record::Record,
    scratchv2::virtual_platter::{INanos, PlatterSample, ReadablePlatter, UNanos},
};

/// time that (approximately) takes to remove the lag between virtual platter and audio playback.
///
/// longer time reduces speed wobbles
static SYNC_TIME: Duration = Duration::from_millis(2000);

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor<R> {
    platter: ReadablePlatter,
    sample_rate: usize,
    record: R,
    /// timestamp and nanosecond of last sample played
    last_played: PlatterSample,
    //// Second newest measurement of virtual playhead
    first_measurement: PlatterSample,
    /// Newest observed virtual playhead position
    second_measurement: PlatterSample,
    /// filtered nanoseconds of playback played per block
    filtered_nanos_played: INanos,
    /// filtered lag of played nanos behind last observed nanos
    filtered_lag: INanos,
}

fn ma_filter(old_value: f64, new_value: f64, new_value_proportion: f64) -> f64 {
    new_value_proportion * new_value + (1. - new_value_proportion) * old_value
}

impl<R: Record> PlatterAudioProcessor<R> {
    fn block_dur(&self, buffer_size: usize) -> Duration {
        Duration::from_secs_f64((buffer_size as f64 / 2.) / (self.sample_rate as f64))
    }

    pub fn new(record: R, sample_rate: usize, platter: ReadablePlatter) -> Self {
        let first_measurement = platter.get_playhead();
        let second_measurement = PlatterSample {
            timestamp_nanos: UNanos(first_measurement.timestamp_nanos.0 + 1),
            record_pos: first_measurement.record_pos,
        };
        let processor = PlatterAudioProcessor {
            platter,
            sample_rate,
            record,
            last_played: first_measurement,
            second_measurement,
            first_measurement,
            filtered_nanos_played: INanos(0), // one of sources of slow startup
            filtered_lag: INanos(0),
        };
        processor
    }

    fn update_measurements(&mut self, cur: PlatterSample) {
        if self.second_measurement.timestamp_nanos != cur.timestamp_nanos {
            self.first_measurement = self.second_measurement;
            self.second_measurement = cur;
        }
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

        self.update_measurements(self.platter.get_playhead());

        let block_duration = self.block_dur(data.len());

        let target_timestamp = {
            let target_timestamp =
                UNanos(self.last_played.timestamp_nanos.0 + block_duration.as_nanos() as u64);

            {
                // target_timestamp may lag behind or rush in front of current virtual platter measurement
                let lags_behind = INanos(
                    self.second_measurement.timestamp_nanos.0 as i64 - target_timestamp.0 as i64,
                );

                // we must filter the lag because if observations arrive less frequently then write_frames,
                // then lag will jump back and forth
                // todo: make this invariant of buffer size using dt and exponent
                self.filtered_lag =
                    INanos(ma_filter(self.filtered_lag.0 as f64, lags_behind.0 as f64, 0.1) as i64);

                self.filtered_lag = self
                    .filtered_lag
                    .clamp(INanos(-5_000_000), INanos(5_000_000)); // think of good numbers
            }

            // if we add lags_behind to target_timestamp, we will remove the lag.
            // we do it slowly (approximately in SYNC_TIME seconds)
            let lags_behind_step = INanos(
                self.filtered_lag.0 / ((SYNC_TIME.as_nanos() / block_duration.as_nanos()) as i64),
            );
            UNanos((target_timestamp.0 as i64 + lags_behind_step.0) as u64)
        };

        let target_playhead_nanos = {
            let target_playhead_raw = INanos(Linear::interpolate_two(
                self.first_measurement.timestamp_nanos.0,
                self.first_measurement.record_pos.0 as f64,
                self.second_measurement.timestamp_nanos.0,
                self.second_measurement.record_pos.0 as f64,
                1., // if interpolation can't be done, assume normal playback speed = 1
                target_timestamp.0,
            ) as i64);
            let played_nanos_raw = INanos(target_playhead_raw.0 - self.last_played.record_pos.0);

            let alpha = 0.8; // higher - snappier
            // todo: make this invariant of buffer size using dt and exponent
            self.filtered_nanos_played = INanos(ma_filter(
                self.filtered_nanos_played.0 as f64,
                played_nanos_raw.0 as f64,
                alpha,
            ) as i64);
            INanos(self.last_played.record_pos.0 + self.filtered_nanos_played.0)
        };

        let mut playhead = self.nanosecs_to_sample(self.last_played.record_pos);
        let target_playhead = self.nanosecs_to_sample(target_playhead_nanos);
        let step = (target_playhead - playhead) / samples_n;

        log::debug!(
            "observations at {}, ..+{}ms",
            self.first_measurement.timestamp_nanos.0,
            (self.second_measurement.timestamp_nanos.0 - self.first_measurement.timestamp_nanos.0)
                / 1000000
        );
        log::debug!(
            "playing at = {:.0}, ..+{:.0}ms",
            self.last_played.timestamp_nanos.0,
            (target_timestamp.0 - self.last_played.timestamp_nanos.0) / 1000000
        );

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
    }
}
