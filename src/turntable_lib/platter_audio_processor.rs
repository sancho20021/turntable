use std::{sync::Arc, time::Duration};

use rtrb::{Consumer, Producer};

use crate::{
    audio_health::DeckHealth,
    decoder::SAMPLE_RATE,
    filters::FirstOrderLPF,
    record::{INanos, Record, UNanos, interpolation::Linear},
    stereo_frame::StereoFrame,
    virtual_platter::{PlatterSample, ReadablePlatter},
};

/// time that (approximately) takes to remove the lag between virtual platter and audio playback.
///
/// longer time reduces speed wobbles
static SYNC_TIME: Duration = Duration::from_millis(2000);

static PLAYHEAD_LPF_TAU: f64 = 0.025;

/// How far the filtered lag is allowed to run before the correction saturates.
/// Hitting it caps the catch-up rate; see
/// [`DeckHealth::blocks_with_lag_correction_maxed`].
static LAG_CLAMP: INanos = INanos(5_000_000);

/// Communication channels for audio processor
pub struct AudioProcessorHandles {
    /// record sent to play instead of current record
    pub next_record: Consumer<Record>,
    /// sink for used records to avoid dropping in audio thread
    pub used_records: Producer<Record>,
    /// source of platter position
    pub platter: ReadablePlatter,
    /// where this deck reports its own health, see [`crate::audio_health`]
    pub health: Arc<DeckHealth>,
}

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor {
    handles: AudioProcessorHandles,
    cur_record: Option<Record>,
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
    /// this deck's health metrics
    health: Arc<DeckHealth>,
}

/// Hands the decoded track to the tray rather than freeing it here.
///
/// A processor is only ever dropped when the stream is torn down, and that runs
/// on the RT-promoted audio thread: cpal's PipeWire worker owns our callback and
/// drops it as it exits, while the main thread waits in `Stream::drop`. Freeing
/// a track there - around 100MB for a four-minute file, measured at 5.3ms - is
/// more continuous CPU than the `RLIMIT_RTTIME` that RT promotion carries (one
/// buffer period: 5.3ms at 256 frames, 667us at 32), and the kernel answers with
/// SIGXCPU: terminate and dump core.
///
/// So it goes down the same disposal ring a live record swap uses, see
/// [`PlatterAudioProcessor::set_record`]. Pushing moves only the `Vec`'s pointer,
/// length and capacity; the tray thread does the actual freeing, and teardown
/// joins it after the stream for exactly that reason.
///
/// Leaking is the fallback, not the plan: with the tray's consumer already gone
/// our producer would be the ring's last owner, and deallocating it would drop
/// the record here - the very SIGXCPU above. A moment before the process exits,
/// letting the OS reclaim the pages is the cheaper mistake.
impl Drop for PlatterAudioProcessor {
    fn drop(&mut self) {
        let Some(record) = self.cur_record.take() else {
            return;
        };

        if self.handles.used_records.is_abandoned() {
            std::mem::forget(record);
            return;
        }

        match self.handles.used_records.push(record) {
            Ok(()) => {}
            Err(rtrb::PushError::Full(record)) => std::mem::forget(record),
        }
    }
}

fn ma_filter(old_value: f64, new_value: f64, new_value_proportion: f64) -> f64 {
    new_value_proportion * new_value + (1. - new_value_proportion) * old_value
}

impl PlatterAudioProcessor {
    pub fn frames_to_dur(buffer_frames_n: usize) -> Duration {
        Duration::from_secs_f64((buffer_frames_n as f64) / (SAMPLE_RATE as f64))
    }

    pub fn frames_to_dur_nanos(buffer_frames_n: usize) -> UNanos {
        let dur = Duration::from_secs_f64((buffer_frames_n as f64) / (SAMPLE_RATE as f64));
        UNanos(dur.as_nanos() as u64)
    }

    fn block_dur(&self, buffer_frames_n: usize) -> Duration {
        Self::frames_to_dur(buffer_frames_n)
    }

    fn set_record(&mut self, record: Record) {
        if let Some(old_record) = self.cur_record.replace(record) {
            // in case of failure, we will drop it in audio thread which frees the
            // whole decoded track here and stalls the callback
            match self.handles.used_records.push(old_record) {
                Ok(()) => {}
                Err(rtrb::PushError::Full(old_rec)) => {
                    self.health.track_freed_on_audio_thread();
                    drop(old_rec);
                }
            }
        }
    }

    pub fn new(handles: AudioProcessorHandles) -> Self {
        let health = Arc::clone(&handles.health);
        let first_measurement = handles.platter.get_playhead();
        let second_measurement = PlatterSample {
            timestamp_nanos: UNanos(first_measurement.timestamp_nanos.0 + 1),
            record_pos: first_measurement.record_pos,
        };
        let processor = PlatterAudioProcessor {
            handles,
            cur_record: None,
            last_played: first_measurement,
            second_measurement,
            first_measurement,
            filtered_target_playhead: FirstOrderLPF::new(PLAYHEAD_LPF_TAU),
            filtered_lag: INanos(0),
            last_speed: 0., // one of sources of slow startup
            health,
        };
        processor
    }

    fn update_measurements(&mut self, cur: PlatterSample) {
        if self.second_measurement.timestamp_nanos != cur.timestamp_nanos {
            self.first_measurement = self.second_measurement;
            self.second_measurement = cur;
        } else {
            // no fresh platter sample, so the slope below is stale and the
            // extrapolation is guessing
            self.health.stale_platter_sample();
        }
    }
    /// Converts sample number to nanosecs
    pub fn sample_to_nanos(&self, sample: f64) -> f64 {
        sample / SAMPLE_RATE as f64 * 1_000_000_000.
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, frames: &mut [StereoFrame]) {
        self.health.block_processed();
        let samples_n = frames.len() as i64;
        if let Ok(record) = self.handles.next_record.pop() {
            self.set_record(record);
        }

        self.update_measurements(self.handles.platter.get_playhead());

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

                    let unclamped = self.filtered_lag;
                    self.filtered_lag =
                        unclamped.clamp(INanos(-LAG_CLAMP.0), LAG_CLAMP); // TODO: think of good numbers
                    if self.filtered_lag != unclamped {
                        self.health.lag_correction_maxed();
                    }

                    // the raw lag is the gauge worth watching: it is both the
                    // audible offset and the gain on slope noise below
                    self.health.set_playback_lag(lags_behind.0);
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
    }
}
