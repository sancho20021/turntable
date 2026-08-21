use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};

use crate::{
    decoder::SAMPLE_RATE,
    filters::FirstOrderLPF,
    record::{INanos, Record, UNanos, interpolation::Linear},
    scratchv2::virtual_platter::{PlatterSample, ReadablePlatter},
    stereo_frame::StereoFrame,
};

/// time that (approximately) takes to remove the lag between virtual platter and audio playback.
///
/// longer time reduces speed wobbles
static SYNC_TIME: Duration = Duration::from_millis(2000);

static PLAYHEAD_LPF_TAU: f64 = 0.025;

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor {
    platter: ReadablePlatter,
    /// record sent to play instead of current record
    next_record: Consumer<Record>,
    cur_record: Option<Record>,
    /// sink for used records to avoid dropping in audio thread
    used_records: Producer<Record>,
    /// timestamp and nanosecond of last sample played
    last_played: PlatterSample,
    //// Second newest measurement of virtual playhead
    first_measurement: PlatterSample,
    /// Newest observed virtual playhead position. strictly newer than first_measurement
    second_measurement: PlatterSample,
    /// filtered target playhead in nanos
    filtered_target_playhead: FirstOrderLPF,
    /// filtered lag of played nanos behind last observed nanos
    filtered_lag: INanos,
    /// last speed of playback
    last_speed: f64,
}

fn ma_filter(old_value: f64, new_value: f64, new_value_proportion: f64) -> f64 {
    new_value_proportion * new_value + (1. - new_value_proportion) * old_value
}

impl PlatterAudioProcessor {
    fn block_duration(buffer_frames_n: usize) -> Duration {
        Duration::from_secs_f64((buffer_frames_n as f64) / (SAMPLE_RATE as f64))
    }

    /// Calculates optimal update frequency for virtual platter
    pub fn platter_update_freq(buffer_frames_n: usize) -> usize {
        (1. / Self::block_duration(buffer_frames_n).as_secs_f64() * 3.) as usize
    }

    fn block_dur(&self, buffer_frames_n: usize) -> Duration {
        Self::block_duration(buffer_frames_n)
    }

    fn set_record(&mut self, record: Record) {
        if let Some(old_record) = self.cur_record.replace(record) {
            // in case of failure, we will drop it in audio thread which may lead to buffer overrun / underrun
            match self.used_records.push(old_record) {
                Ok(()) => {}
                Err(rtrb::PushError::Full(old_rec)) => {
                    log::warn!(
                        "Failed to send used record to record changer, dropping on audio thread (may cause buffer overrun / underrun)"
                    );
                    drop(old_rec);
                }
            }
        }
    }

    pub fn new(
        platter: ReadablePlatter,
        next_record: Consumer<Record>,
        used_records: Producer<Record>,
    ) -> Self {
        let first_measurement = platter.get_playhead();
        let second_measurement = PlatterSample {
            timestamp_nanos: UNanos(first_measurement.timestamp_nanos.0 + 1),
            record_pos: first_measurement.record_pos,
        };
        let processor = PlatterAudioProcessor {
            platter,
            cur_record: None,
            last_played: first_measurement,
            second_measurement,
            first_measurement,
            filtered_target_playhead: FirstOrderLPF::new(PLAYHEAD_LPF_TAU),
            filtered_lag: INanos(0),
            next_record,
            used_records,
            last_speed: 0., // one of sources of slow startup
        };
        processor
    }

    fn update_measurements(&mut self, cur: PlatterSample) {
        if self.second_measurement.timestamp_nanos != cur.timestamp_nanos {
            self.first_measurement = self.second_measurement;
            self.second_measurement = cur;
        }
    }
    /// Converts sample number to nanosecs
    pub fn sample_to_nanos(&self, sample: f64) -> f64 {
        sample / SAMPLE_RATE as f64 * 1_000_000_000.
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, frames: &mut [StereoFrame]) {
        let start = Instant::now();
        let samples_n = frames.len() as i64;
        if let Ok(record) = self.next_record.pop() {
            self.set_record(record);
        }

        self.update_measurements(self.platter.get_playhead());

        let block_duration = self.block_dur(frames.len());

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
            let playhead = INanos(
                self.second_measurement.record_pos.0
                    - (self.last_speed * block_duration.as_nanos() as f64) as i64,
            );

            {
                // without this lag subtraction, filter will fallback to warm-up state, and produce playback ramping
                let speed_nanos_per_sec = self.last_speed * 1_000_000_000.0;
                let steady_state_lag_nanos = speed_nanos_per_sec * PLAYHEAD_LPF_TAU;

                self.filtered_target_playhead.force_state(
                    self.second_measurement.record_pos.0 as f64 - steady_state_lag_nanos,
                );
            };
            self.filtered_lag = INanos(0);

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
                    self.filtered_lag =
                        INanos(
                            ma_filter(self.filtered_lag.0 as f64, lags_behind.0 as f64, 0.1) as i64,
                        );

                    self.filtered_lag = self
                        .filtered_lag
                        .clamp(INanos(-5_000_000), INanos(5_000_000)); // TODO: think of good numbers

                    log::debug!("raw lag = {}ms", lags_behind.as_millis());
                    log::debug!("filtered lag = {}ms", self.filtered_lag.as_millis());
                }

                // if we add lags_behind to target_timestamp, we will remove the lag.
                // we do it slowly (approximately in SYNC_TIME seconds)
                let lags_behind_step = INanos(
                    self.filtered_lag.0
                        / ((SYNC_TIME.as_nanos() / block_duration.as_nanos()) as i64),
                );
                log::debug!("stepped filtered lag = {}", lags_behind_step.as_millis());
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

            let target_playhead_nanos = INanos(self.filtered_target_playhead.advance(
                block_duration.as_secs_f64(),
                target_playhead_estimated.0 as f64,
            ) as i64);

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
            "observations at {}ms, ..+{}ms",
            self.first_measurement.timestamp_nanos.as_millis(),
            (self.second_measurement.timestamp_nanos.0 - self.first_measurement.timestamp_nanos.0)
                / 1000000
        );
        log::debug!(
            "playing at = {}ms, ..+{:.0}ms at speed {:.2}",
            self.last_played.timestamp_nanos.as_millis(),
            (target_playhead.timestamp_nanos.0 - self.last_played.timestamp_nanos.0) / 1000000,
            step.0 as f64 / (self.sample_to_nanos(1.))
        );

        self.last_speed =
            (target_playhead.record_pos.0 - playhead.0) as f64 / block_duration.as_nanos() as f64;

        self.last_played = target_playhead;

        if let Some(rec) = &self.cur_record {
            for frame in frames {
                *frame = rec.get_sample(playhead);
                playhead.0 += step.0;
            }
        } else {
            frames.fill(StereoFrame::default());
        }

        let elapsed = start.elapsed();
        if elapsed > Duration::from_micros(1500) {
            log::warn!(
                "write frames took {}us at timestamp={}us",
                elapsed.as_micros(),
                self.last_played.timestamp_nanos.as_micros()
            );
        }
    }
}
