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
    /// clock and position of the last block played, `None` until the first one
    last_played: Option<PlatterSample>,
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
/// A processor is dropped on the RT-promoted audio thread, because cpal's
/// PipeWire worker owns our callback and drops it as it exits. Freeing a track
/// costs more continuous CPU than the `RLIMIT_RTTIME` that RT promotion carries,
/// and the kernel answers with SIGXCPU, so it goes down the same disposal ring as
/// a live swap: see [`PlatterAudioProcessor::set_record`]. Teardown joins the
/// tray after the stream so it is still there to receive it.
///
/// The fallback leaks because with the tray's consumer gone our producer would be
/// the ring's last owner, and deallocating it would drop the record here.
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
            last_played: None,
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
        self.health.callback_rendered();
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

        let jumped = observed_played_nanos.0.abs() > JUMP_THRESHOLD.0;

        let (playhead_start, target_playhead) = match self.last_played {
            Some(last_played) if !jumped => {
                let target_timestamp = {
                    let target_timestamp =
                        UNanos(last_played.timestamp_nanos.0 + block_duration.as_nanos() as u64);

                    {
                        // target_timestamp may lag behind or rush in front of current virtual platter measurement
                        let lags_behind = INanos(
                            self.second_measurement.timestamp_nanos.0 as i64
                                - target_timestamp.0 as i64,
                        );

                        // we must filter the lag because if observations arrive less frequently then write_frames,
                        // then lag will jump back and forth
                        self.filtered_lag = INanos(ma_filter(
                            self.filtered_lag.0 as f64,
                            lags_behind.0 as f64,
                            0.1,
                        ) as i64);

                        let unclamped = self.filtered_lag;
                        self.filtered_lag = unclamped.clamp(INanos(-LAG_CLAMP.0), LAG_CLAMP); // TODO: think of good numbers
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

                (
                    INanos(last_played.record_pos.0),
                    PlatterSample {
                        timestamp_nanos: target_timestamp,
                        record_pos: target_playhead_nanos,
                    },
                )
            }

            // Anchor on the latest observation, whatever the current lag. Either
            // the playhead jumped, or nothing has played yet and the clock seeded
            // at construction is stale by however long the device took to open.
            _ => {
                if jumped {
                    self.health.playhead_jumped();
                }
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

                (playhead, self.second_measurement)
            }
        };

        self.last_speed = (target_playhead.record_pos.0 - playhead_start.0) as f64
            / block_duration.as_nanos() as f64;

        self.last_played = Some(target_playhead);

        if let Some(rec) = &self.cur_record {
            // Stepping in fractional samples, not whole nanoseconds: a truncated
            // integer step lands short of the target, leaving the next block to
            // start from a position this one never reached.
            let mut position = rec.nanosecs_to_sample(playhead_start);
            let target = rec.nanosecs_to_sample(target_playhead.record_pos);
            let step = (target - position) / samples_n as f64;

            for frame in frames {
                *frame = rec.get_sample_at(position);
                position += step;
            }
        } else {
            frames.fill(StereoFrame::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_health::AudioHealth;
    use crate::record::interpolation::Interpolator;
    use crate::virtual_platter::{WritablePlatter, new_platter};

    const FRAMES: usize = 64;

    /// A record whose sample value is its own index.
    ///
    /// Interpolating a straight line is exact, so a frame read back at
    /// fractional position `p` carries `p`: the output *is* the playhead the
    /// render loop used. Kept short so every index is exact in `f32`.
    fn ramp_record(len: usize) -> Record {
        let samples = (0..len)
            .map(|i| StereoFrame {
                l: i as f32,
                r: 0.0,
            })
            .collect();
        Record::new(samples, Interpolator::linear())
    }

    fn processor_on_a_ramp() -> (PlatterAudioProcessor, WritablePlatter) {
        let (writable, readable) = new_platter();
        let (mut records_in, next_record) = rtrb::RingBuffer::new(1);
        let (used_records, _records_out) = rtrb::RingBuffer::new(3);

        let mut processor = PlatterAudioProcessor::new(AudioProcessorHandles {
            next_record,
            used_records,
            platter: readable,
            health: AudioHealth::new(FRAMES as u32, SAMPLE_RATE, 1).deck(0),
        });
        records_in.push(ramp_record(200_000)).unwrap();
        // the record is picked up on the next callback
        processor.write_frames(&mut [StereoFrame::default(); FRAMES]);
        (processor, writable)
    }

    /// Advances the platter one block at normal speed and renders it.
    fn render_block(
        processor: &mut PlatterAudioProcessor,
        platter: &mut WritablePlatter,
        block: usize,
    ) -> Vec<StereoFrame> {
        let block_nanos = PlatterAudioProcessor::frames_to_dur_nanos(FRAMES).0;
        let elapsed = block_nanos * block as u64;
        platter.update_playhead(INanos(elapsed as i64), UNanos(elapsed));

        let mut frames = vec![StereoFrame::default(); FRAMES];
        processor.write_frames(&mut frames);
        frames
    }

    /// The loop must finish where `last_played` says it did, or the next block
    /// starts from a position this one never rendered.
    #[test]
    fn the_block_lands_on_the_position_it_records() {
        let (mut processor, mut platter) = processor_on_a_ramp();

        for block in 1..12 {
            let frames = render_block(&mut processor, &mut platter, block);

            let step = (frames[1].l - frames[0].l) as f64;
            if step <= 0.0 {
                continue; // still spinning up; speed has not become positive yet
            }
            let walked_to = frames[FRAMES - 1].l as f64 + step;

            let recorded = processor.last_played.expect("a block has been played");
            let target = SAMPLE_RATE as f64 * (recorded.record_pos.0 as f64 / 1e9);

            // one truncated nanosecond step used to leave this short by up to
            // `FRAMES` nanoseconds, which is ~0.003 samples at 64 frames
            assert!(
                (walked_to - target).abs() < 1e-3,
                "block {block}: loop walked to {walked_to} but recorded {target}"
            );
        }
    }

    /// Within a block every sample must be one even step from the last.
    #[test]
    fn the_playhead_advances_evenly_inside_a_block() {
        let (mut processor, mut platter) = processor_on_a_ramp();

        for block in 1..12 {
            let frames = render_block(&mut processor, &mut platter, block);
            let step = (frames[1].l - frames[0].l) as f64;
            if step <= 0.0 {
                continue;
            }

            for i in 1..FRAMES {
                let actual = (frames[i].l - frames[i - 1].l) as f64;
                assert!(
                    (actual - step).abs() < 1e-3,
                    "block {block} frame {i}: step {actual} vs {step}"
                );
            }
        }
    }

    /// Nothing has played yet, so there is no clock to carry forward.
    #[test]
    fn the_first_callback_has_no_previous_block() {
        let (writable, readable) = new_platter();
        let (_records_in, next_record) = rtrb::RingBuffer::new(1);
        let (used_records, _records_out) = rtrb::RingBuffer::new(3);
        drop(writable);

        let processor = PlatterAudioProcessor::new(AudioProcessorHandles {
            next_record,
            used_records,
            platter: readable,
            health: AudioHealth::new(FRAMES as u32, SAMPLE_RATE, 1).deck(0),
        });
        assert!(processor.last_played.is_none());
    }
}
