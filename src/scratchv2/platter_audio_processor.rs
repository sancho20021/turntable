use std::time::Duration;

use crossbeam::channel::Receiver;

use crate::{
    record::{INanos, Record, UNanos, interpolation::Linear},
    scratchv2::virtual_platter::{PlatterSample, ReadablePlatter},
};

/// time that (approximately) takes to remove the lag between virtual platter and audio playback.
///
/// longer time reduces speed wobbles
static SYNC_TIME: Duration = Duration::from_millis(2000);

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor {
    platter: ReadablePlatter,
    sample_rate: usize,
    /// record sent to play instead of current record
    next_record: Receiver<Record>,
    cur_record: Option<Record>,
    /// timestamp and nanosecond of last sample played
    last_played: PlatterSample,
    //// Second newest measurement of virtual playhead
    first_measurement: PlatterSample,
    /// Newest observed virtual playhead position. strictly newer than first_measurement
    second_measurement: PlatterSample,
    /// filtered nanoseconds of playback played per block
    filtered_nanos_played: INanos,
    /// filtered lag of played nanos behind last observed nanos
    filtered_lag: INanos,
}

fn ma_filter(old_value: f64, new_value: f64, new_value_proportion: f64) -> f64 {
    new_value_proportion * new_value + (1. - new_value_proportion) * old_value
}

impl PlatterAudioProcessor {
    fn block_dur(&self, buffer_size: usize) -> Duration {
        Duration::from_secs_f64((buffer_size as f64 / 2.) / (self.sample_rate as f64))
    }

    fn set_record(&mut self, record: Record) {
        self.cur_record = Some(record);
    }

    pub fn new(
        sample_rate: usize,
        platter: ReadablePlatter,
        next_record: Receiver<Record>,
    ) -> Self {
        let first_measurement = platter.get_playhead();
        let second_measurement = PlatterSample {
            timestamp_nanos: UNanos(first_measurement.timestamp_nanos.0 + 1),
            record_pos: first_measurement.record_pos,
        };
        let processor = PlatterAudioProcessor {
            platter,
            sample_rate,
            cur_record: None,
            last_played: first_measurement,
            second_measurement,
            first_measurement,
            filtered_nanos_played: INanos(0), // one of sources of slow startup
            filtered_lag: INanos(0),
            next_record,
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
        if let Ok(record) = self.next_record.try_recv() {
            println!("Track loaded");
            self.set_record(record);
        }

        let samples_n = data.len() as i64 / 2;

        self.update_measurements(self.platter.get_playhead());

        let block_duration = self.block_dur(data.len());

        // to detect reset, rewind, fast-forward, etc
        const JUMP_THRESHOLD: INanos = INanos(500_000_000);
        let observed_played_nanos =
            INanos(self.second_measurement.record_pos.0 - self.first_measurement.record_pos.0);

        let (mut playhead, target_playhead, step) = if observed_played_nanos.0.abs()
            > JUMP_THRESHOLD.0
        {
            // =================================================================
            // JUMP case, we jump to the latest observation no matter how big is the current lag
            // =================================================================
            log::info!(
                "Playhead jumped: jump distance: {:.2}s",
                observed_played_nanos.0 as f64 / 1_000_000_000.0
            );
            let playhead =
                INanos(self.second_measurement.record_pos.0 - self.filtered_nanos_played.0);

            // here we lose about maximum samples_n nanoseconds of accumulated error, which is fine as
            // sums up to around 44ns of drift per second
            let step = INanos((self.second_measurement.record_pos.0 - playhead.0) / samples_n);
            (playhead, self.second_measurement, step)
        } else {
            let target_timestamp = {
                let target_timestamp =
                    UNanos(self.last_played.timestamp_nanos.0 + block_duration.as_nanos() as u64);

                {
                    // target_timestamp may lag behind or rush in front of current virtual platter measurement
                    let lags_behind = INanos(
                        self.second_measurement.timestamp_nanos.0 as i64
                            - target_timestamp.0 as i64,
                    );

                    // we must filter the lag because if observations arrive less frequently then write_frames,
                    // then lag will jump back and forth
                    // todo: make this invariant of buffer size using dt and exponent
                    self.filtered_lag =
                        INanos(
                            ma_filter(self.filtered_lag.0 as f64, lags_behind.0 as f64, 0.1) as i64,
                        );

                    self.filtered_lag = self
                        .filtered_lag
                        .clamp(INanos(-5_000_000), INanos(5_000_000)); // TODO: think of good numbers
                }

                // if we add lags_behind to target_timestamp, we will remove the lag.
                // we do it slowly (approximately in SYNC_TIME seconds)
                let lags_behind_step = INanos(
                    self.filtered_lag.0
                        / ((SYNC_TIME.as_nanos() / block_duration.as_nanos()) as i64),
                );
                UNanos((target_timestamp.0 as i64 + lags_behind_step.0) as u64)
            };

            let target_playhead_estimated = INanos(Linear::interpolate_two(
                self.first_measurement.timestamp_nanos.0,
                self.first_measurement.record_pos.0 as f64,
                self.second_measurement.timestamp_nanos.0,
                self.second_measurement.record_pos.0 as f64,
                1., // if interpolation can't be done, assume normal playback speed = 1
                target_timestamp.0,
            ) as i64);
            let to_play_estimated =
                INanos(target_playhead_estimated.0 - self.last_played.record_pos.0);

            let target_playhead_nanos = {
                let alpha = 0.8; // higher - snappier
                // todo: make this invariant of buffer size using dt and exponent
                self.filtered_nanos_played = INanos(ma_filter(
                    self.filtered_nanos_played.0 as f64,
                    to_play_estimated.0 as f64,
                    alpha,
                ) as i64);
                INanos(self.last_played.record_pos.0 + self.filtered_nanos_played.0)
            };

            let playhead_nanos = INanos(self.last_played.record_pos.0);
            // here we lose about maximum samples_n nanoseconds of accumulated error, which is fine as
            // sums up to around 44ns of drift per second
            let step_nanos = INanos((target_playhead_nanos.0 - playhead_nanos.0) / samples_n);
            (
                playhead_nanos,
                PlatterSample {
                    timestamp_nanos: target_timestamp,
                    record_pos: target_playhead_nanos,
                },
                step_nanos,
            )
        };

        log::debug!(
            "observations at {}, ..+{}ms",
            self.first_measurement.timestamp_nanos.0,
            (self.second_measurement.timestamp_nanos.0 - self.first_measurement.timestamp_nanos.0)
                / 1000000
        );
        log::debug!(
            "playing at = {:.0}, ..+{:.0}ms at speed {:.2}",
            self.last_played.timestamp_nanos.0,
            (target_playhead.timestamp_nanos.0 - self.last_played.timestamp_nanos.0) / 1000000,
            step.0 as f64 / (self.sample_to_nanos(1.))
        );

        self.last_played = target_playhead;

        if let Some(rec) = &self.cur_record {
            for frame in data.chunks_mut(2) {
                let sample = rec.get_sample(playhead);
                frame[0] = sample.l;
                frame[1] = sample.r;
                playhead.0 += step.0;
            }
        } else {
            data.fill(0.);
        }
    }
}
