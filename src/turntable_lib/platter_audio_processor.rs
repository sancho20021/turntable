use std::{sync::Arc, time::Duration};

use rtrb::{Consumer, Producer};

use crate::{
    audio_health::DeckHealth,
    decoder::SAMPLE_RATE,
    filters::{FirstOrderLPF, StereoDcBlocker},
    record::{INanos, Record, UNanos, interpolation::Linear},
    stereo_frame::StereoFrame,
    virtual_platter::{PlatterSample, ReadablePlatter},
};

/// time that (approximately) takes to remove the lag between virtual platter and audio playback.
///
/// longer time reduces speed wobbles
static SYNC_TIME: Duration = Duration::from_millis(2000);

static PLAYHEAD_LPF_TAU: f64 = 0.025;
// static PLAYHEAD_LPF_TAU: f64 = 0.05;

/// Corner of the output high-pass, in Hz.
///
/// A parked playhead renders the same sample over and over, so a deck sitting
/// still holds whatever level it stopped on - up to full scale - as a DC offset,
/// and the step down to silence when the stream closes is a click the offset was
/// storing up. Draining it here is also the more faithful deck: a cartridge is a
/// velocity transducer, so a record at rest puts out nothing, and one moving
/// slower than this corner puts out proportionally less.
///
/// Lower it to keep more sub-bass, at the
/// price of a slower drain.
static DC_BLOCKER_HZ: f64 = 10.;

/// How long a declick fades in for. declick is for seeking and record swapping.
/// Capped at the block.
static DECLICK: Duration = Duration::from_millis(3);

/// How far the filtered lag is allowed to run before the correction saturates.
/// Hitting it caps the catch-up rate; see
/// [`DeckHealth::callbacks_with_lag_correction_maxed`].
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
    /// drains the DC a still playhead would otherwise hold, see [`DC_BLOCKER_HZ`]
    dc_blocker: StereoDcBlocker,
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
            dc_blocker: StereoDcBlocker::new(DC_BLOCKER_HZ, SAMPLE_RATE),
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

    /// The frame this deck last put out, read back off the record it came from.
    fn last_emitted(&self) -> StereoFrame {
        match (&self.cur_record, self.last_played) {
            (Some(record), Some(played)) => record.get_sample(played.record_pos),
            _ => StereoFrame::default(),
        }
    }

    /// Samples a declick fades over: [`DECLICK`], capped at the block.
    fn declick_frames(buffer_frames: usize) -> usize {
        let wanted = (DECLICK.as_secs_f64() * SAMPLE_RATE as f64) as usize;
        wanted.min(buffer_frames)
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, frames: &mut [StereoFrame]) {
        self.health.callback_rendered();
        let samples_n = frames.len() as i64;

        // Frame the block fades in from, once something has torn the stream.
        // Read while the record it belongs to is still in hand, which for a swap
        // means before the incoming one replaces it.
        let mut declick_from = None;

        let incoming = self.handles.next_record.pop().ok();
        if let Some(record) = incoming {
            declick_from = Some(self.last_emitted());
            self.set_record(record);
        }

        self.update_measurements(self.handles.platter.get_playhead());

        let block_duration = self.block_dur(frames.len());

        // to detect reset, rewind, fast-forward, etc
        const JUMP_THRESHOLD: INanos = INanos(500_000_000);
        let observed_played_nanos =
            INanos(self.second_measurement.record_pos.0 - self.first_measurement.record_pos.0);

        let jumped = observed_played_nanos.0.abs() > JUMP_THRESHOLD.0;
        if jumped {
            declick_from = declick_from.or_else(|| Some(self.last_emitted()));
        }

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

                // Where the anchored block lands. An LPF tracking a ramp settles
                // a `slope * tau` behind its input, so this is the offset the
                // rendered playhead already runs at while a deck plays: landing
                // on it is landing in steady state. Without the subtraction the
                // filter would instead restart from a cold state and ramp the
                // playback in.
                let speed_nanos_per_sec = self.last_speed * 1_000_000_000.0;
                let steady_state_lag_nanos = speed_nanos_per_sec * PLAYHEAD_LPF_TAU;
                let anchor =
                    INanos(self.second_measurement.record_pos.0 - steady_state_lag_nanos as i64);

                self.filtered_target_playhead.force_state(anchor.0 as f64);
                self.filtered_lag = INanos(0);

                let playhead =
                    INanos(anchor.0 - (self.last_speed * block_duration.as_nanos() as f64) as i64);

                (
                    playhead,
                    PlatterSample {
                        timestamp_nanos: self.second_measurement.timestamp_nanos,
                        record_pos: anchor,
                    },
                )
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

            for frame in &mut *frames {
                *frame = rec.get_sample_at(position);
                position += step;
            }
        } else {
            frames.fill(StereoFrame::default());
        }

        // A jump or a swap leaves the block starting on a sample unrelated to
        // the one the last block ended on, and a step edge is a click. Fading in
        // from that last frame turns the step into a slope
        if let Some(from) = declick_from {
            let ramp = Self::declick_frames(frames.len());
            for (i, frame) in frames[..ramp].iter_mut().enumerate() {
                // (i + 1) / ramp, so the fade is complete at the end of the
                // window and leaves no pedestal to step off
                let new_share = (i + 1) as f32 / ramp as f32;
                frame.l = from.l + (frame.l - from.l) * new_share;
                frame.r = from.r + (frame.r - from.r) * new_share;
            }
        }

        for frame in frames {
            *frame = self.dc_blocker.advance(*frame);
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
        // These tests read the playhead off the samples, which the output
        // high-pass would filter along with everything else.
        processor.dc_blocker = StereoDcBlocker::bypass();
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

    /// Peak of the test tones, -6dBFS, so a step off one is a known size.
    const TONE_PEAK: f64 = 0.5;

    /// A sine, both channels alike. Smooth by construction: the only steps in a
    /// stream rendered from one are steps the deck put there.
    fn tone(secs: f64, hz: f64, peak: f64) -> Record {
        let n = (secs * SAMPLE_RATE as f64) as usize;
        let samples = (0..n)
            .map(|i| {
                let t = i as f64 / SAMPLE_RATE as f64;
                let v = (peak * (std::f64::consts::TAU * hz * t).sin()) as f32;
                StereoFrame { l: v, r: v }
            })
            .collect();
        Record::new(samples, Interpolator::linear())
    }

    /// A deck sitting still must not hold the sample it stopped on: that offset
    /// is what the stream's close steps off, and it is a click the size of
    /// whatever the playhead was parked on.
    #[test]
    fn a_parked_playhead_drains_to_silence() {
        let (mut writable, readable) = new_platter();
        let (mut records_in, next_record) = rtrb::RingBuffer::new(1);
        let (used_records, _records_out) = rtrb::RingBuffer::new(3);

        let mut processor = PlatterAudioProcessor::new(AudioProcessorHandles {
            next_record,
            used_records,
            platter: readable,
            health: AudioHealth::new(FRAMES as u32, SAMPLE_RATE, 1).deck(0),
        });
        records_in.push(tone(5.0, 440., TONE_PEAK)).unwrap();

        let block_nanos = PlatterAudioProcessor::frames_to_dur_nanos(FRAMES).0;
        let mut frames = vec![StereoFrame::default(); FRAMES];

        // Play long enough for the sync loop to reach steady state.
        for block in 0..200u64 {
            let elapsed = block_nanos * block;
            writable.update_playhead(INanos(elapsed as i64), UNanos(elapsed));
            processor.write_frames(&mut frames);
        }
        let playing_peak = frames.iter().map(|f| f.l.abs()).fold(0., f32::max);
        assert!(
            playing_peak > 0.4,
            "the filter ate the music: peak {playing_peak} of a -6dBFS sine"
        );

        // Park it: the platter clock keeps running while the position holds,
        // which is what the driver does once the speed reaches zero.
        //
        // A second is generous: the playhead converges on the parked position
        // within ~0.2s, and the high-pass has drained what that left by ~0.45s.
        // The platter's own wind-down comes before any of this and is the
        // audible part of a stop.
        let parked_at = INanos((block_nanos * 200) as i64);
        let drain_blocks =
            (1.0 / PlatterAudioProcessor::frames_to_dur(FRAMES).as_secs_f64()).ceil() as u64;
        for block in 200..200 + drain_blocks {
            let elapsed = block_nanos * block;
            writable.update_playhead(parked_at, UNanos(elapsed));
            processor.write_frames(&mut frames);
        }

        let parked_peak = frames.iter().map(|f| f.l.abs()).fold(0., f32::max);
        assert!(
            parked_peak < 1e-6,
            "parked deck still holds {parked_peak} on the output"
        );
    }

    /// The probe these tests read: a 100Hz sine at -6dBFS. Slow and quiet
    /// enough that a step the deck introduces stands out against it.
    const TONE_HZ: f64 = 100.;

    /// The most one output sample may differ from the last.
    ///
    /// Playing the probe undisturbed steps 0.0065 at most - that is simply how
    /// far a 100Hz sine at -6dBFS can move in 1/48000 of a second - so this
    /// allows four times that. A torn seam comes in 5x to 30x over, so the
    /// exact figure is not load bearing. [`undisturbed_playback_is_smooth`] is
    /// what keeps it honest.
    const MAX_STEP: f32 = 0.026;

    /// Blocks played before anything is measured, so no test reads the warm-up:
    /// the first callback has no previous block to carry forward, and the
    /// playhead filter needs settling time. At 64 frames this is 133ms, five
    /// [`PLAYHEAD_LPF_TAU`].
    const WARM_UP: u64 = 100;

    /// Plays a deck up to speed, runs `disturb` between two blocks - where a
    /// jump or a load lands in the app - and returns the worst step between
    /// neighbouring output samples across that seam and the blocks after it.
    ///
    /// A pop lives on the seam *between* two blocks, which nothing looking at
    /// one block at a time can see. `disturb` is handed the platter and the
    /// record ring, the two things the app can change under a running deck.
    fn worst_step_across(
        record: Record,
        disturb: impl FnOnce(&mut WritablePlatter, &mut Producer<Record>),
    ) -> f32 {
        let (mut platter, readable) = new_platter();
        let (mut records, next_record) = rtrb::RingBuffer::new(2);
        // the consumer is held so the processor's disposal ring is not abandoned
        let (used_records, _used) = rtrb::RingBuffer::new(3);

        let mut processor = PlatterAudioProcessor::new(AudioProcessorHandles {
            next_record,
            used_records,
            platter: readable,
            health: AudioHealth::new(FRAMES as u32, SAMPLE_RATE, 1).deck(0),
        });
        records.push(record).expect("a fresh ring has room");

        // Nominal speed is a block of record time for every block of clock time,
        // integrated off whatever the platter currently holds, which is what
        // `PlatterDriver::calculate_position` does while a deck plays. Nothing
        // here remembers the position: the platter does, as in the app.
        let block_nanos = PlatterAudioProcessor::frames_to_dur_nanos(FRAMES).0;
        let mut frames = [StereoFrame::default(); FRAMES];
        let mut clock = UNanos(0);
        let mut advance = |platter: &mut WritablePlatter, clock: &mut UNanos| {
            let at = platter.get_playhead();
            *clock = UNanos(clock.0 + block_nanos);
            platter.update_playhead(INanos(at.record_pos.0 + block_nanos as i64), *clock);
        };

        for _ in 0..WARM_UP {
            advance(&mut platter, &mut clock);
            processor.write_frames(&mut frames);
        }

        let mut prev = frames[FRAMES - 1];
        disturb(&mut platter, &mut records);

        let mut worst = 0.;
        for _ in 0..2 {
            advance(&mut platter, &mut clock);
            processor.write_frames(&mut frames);
            for frame in frames {
                // both channels carry the same tone, so the left speaks for both
                worst = f32::max(worst, (frame.l - prev.l).abs());
                prev = frame;
            }
        }
        worst
    }

    /// The reference the other two are read against: left alone, the deck is as
    /// smooth as the record it reads.
    #[test]
    fn undisturbed_playback_is_smooth() {
        let step = worst_step_across(tone(5., TONE_HZ, TONE_PEAK), |_, _| {});

        assert!(step <= MAX_STEP, "steady playback steps {step:.5}");
    }

    /// A seek reseats the playhead, and the block after it starts on a sample
    /// unrelated to the one the last block ended on.
    ///
    /// The extra half cycle only keeps the landing off a whole number of them,
    /// which would come back in phase and hide the seam completely. It does not
    /// set the step's height - an arbitrary seek into real material lands where
    /// it lands. That the step is there at all is the defect.
    #[test]
    fn a_seek_does_not_tear_the_stream() {
        // well past JUMP_THRESHOLD, so the processor takes its re-anchor branch
        let by = Duration::from_secs(2) + Duration::from_secs_f64(0.5 / TONE_HZ);

        let step = worst_step_across(tone(5., TONE_HZ, TONE_PEAK), |platter, _| {
            // what `PlatterEvent::MovePlayhead` does: the position moves, the
            // clock does not
            let at = platter.get_playhead();
            platter.update_playhead(
                INanos(at.record_pos.0 + by.as_nanos() as i64),
                at.timestamp_nanos,
            );
        });

        assert!(
            step <= MAX_STEP,
            "a seek steps {step:.5}. that step is the pop"
        );
    }

    /// Loading a track onto a running deck swaps the record between one block
    /// and the next. Two tracks are unrelated at that seam; inverting the tone
    /// makes that repeatable - the position carries straight on across the swap,
    /// so the step is exactly twice whatever the waveform was worth there.
    #[test]
    fn loading_a_track_mid_play_does_not_tear_the_stream() {
        let step = worst_step_across(tone(5., TONE_HZ, TONE_PEAK), |_, records| {
            records
                .push(tone(5., TONE_HZ, -TONE_PEAK))
                .expect("the ring has room");
        });

        assert!(
            step <= MAX_STEP,
            "a record swap steps {step:.5}. that step is the pop"
        );
    }
}
