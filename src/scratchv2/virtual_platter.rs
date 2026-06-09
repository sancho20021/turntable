use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer, RingBuffer};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PlatterSample {
    /// When the sample was recorded
    pub time: Instant,
    /// Record position in nanoseconds from the start
    pub record_pos: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PlatterError {
    /// Too old sample requested that is not present in the buffer anymore
    OldSampleRequested {
        /// Oldest available sample
        oldest: PlatterSample,
    },
    /// Too new sample requested that is not present in the buffer yet.
    NewerSampleRequested {
        /// Newest available sample
        newest: PlatterSample,
    },
    /// Buffer didn't contain any samples for an unknown reason
    NoSamples,
}

pub struct VirtualPlatter {
    /// First sample just before the first sample from the queue
    sample0: Option<PlatterSample>,
    /// queue of samples
    samples: Consumer<PlatterSample>,
}

impl VirtualPlatter {
    /// Initializes a concurrent wait and lock free ring buffer
    /// to represent a virtual platter that is to be read by an audio thread
    /// and updated by some virtual platter logic
    ///
    /// - `audio_block_duration`: The exact time span of one audio block (e.g., 512 samples @ 44.1kHz ≈ 11.6ms).
    /// - `update_frequency_hz`: How many times per second the producer spins/spams position updates.
    /// - `jitter_factor`: Safety multiplier to handle OS scheduling noise (2.0 to 3.0 recommended).
    pub fn new(
        audio_block_duration: Duration,
        update_frequency_hz: f64,
        jitter_factor: f64,
    ) -> (Producer<PlatterSample>, VirtualPlatter) {
        // 1. Convert duration to seconds
        let block_duration_secs = audio_block_duration.as_secs_f64();
        println!("block duration = {block_duration_secs:.4}s");

        // 2. Calculate ideal slots: Duration * Frequency * Jitter
        let ideal_capacity = block_duration_secs * update_frequency_hz * jitter_factor;
        println!("ideal capacity = {ideal_capacity:.0}");

        // 3. Round up to the nearest integer and enforce a reasonable minimum floor (e.g., 4 slots)
        let calculated_slots = (ideal_capacity.ceil() as usize).max(4);

        // 4. Round up to the next power of two.
        // While `rtrb` works with any size, power-of-two sizes optimize hardware ring-masking
        // performance and memory alignment.
        let final_capacity = calculated_slots.next_power_of_two();

        // 5. Instantiate the lock-free ring buffer
        let (prod, cons) = RingBuffer::new(final_capacity);
        let plat = Self::_new(cons);
        println!("platter buffer size = {final_capacity}");
        (prod, plat)
    }

    fn _new(queue: Consumer<PlatterSample>) -> Self {
        Self {
            sample0: None,
            samples: queue,
        }
    }

    /// Retrieves two samples just before and just after the requested time.
    ///
    /// Assumes timestamps of samples are monotonically increasing.
    ///
    /// Assumes requested timestamps of calls is monotonically increasing

    /// Fails with:
    /// - `PlatterError::NoSamples` if buffer has no samples,
    /// - `PlatterError::OldSampleRequested` if oldest sample in buffer is younger than requested
    /// - `PlatterError::NewerSampleRequested` if newest sample in buffer is older than requested
    pub fn get_sample(
        &mut self,
        when: Instant,
    ) -> Result<(PlatterSample, PlatterSample), PlatterError> {
        println!("Sample requested for {when:?}");

        // Step 1: Ensure we have a baseline sample0 to start comparing against.
        if self.sample0.is_none() {
            if let Ok(first_sample) = self.samples.pop() {
                self.sample0 = Some(first_sample);
            } else {
                return Err(PlatterError::NoSamples);
            }
        }
        let mut s0 = self.sample0.unwrap(); // safe

        // Step 2: Validate that the requested time isn't older than our history anchor.
        if when < s0.time {
            return Err(PlatterError::OldSampleRequested { oldest: s0 });
        }

        // Step 3: Look for sample1. It must be newer than `when`.
        // We will peek or pop from the queue until we cross the `when` threshold.
        loop {
            match self.samples.peek().cloned() {
                Ok(s1) => {
                    if s1.time > when {
                        // We found a newer sample
                        self.sample0 = Some(s0);
                        return Ok((s0, s1));
                    }
                    // s1 is still too old, pop it and put into s0
                    s0 = self
                        .samples
                        .pop()
                        .map_err(|_| PlatterError::NewerSampleRequested { newest: s0 })?;
                }
                Err(_) => {
                    // The queue is empty, and we haven't found any sample newer than `when`.
                    self.sample0 = Some(s0);
                    return Err(PlatterError::NewerSampleRequested { newest: s0 });
                }
            }
        }
    }
}

/// Linearly interpolates the platter position at a target time between two samples.
/// Returns the interpolated position in seconds.
pub fn interpolate(sample1: PlatterSample, sample2: PlatterSample, target_time: Instant) -> f64 {
    // If the samples are at the exact same time, avoid division by zero
    if sample1.time == sample2.time {
        return (sample1.record_pos as f64 / 1_000_000_000.0) as f64;
    }

    // Calculate the total time delta between the two samples
    let total_duration = sample2.time.duration_since(sample1.time).as_secs_f64();

    // Calculate how far along the target time is from the first sample
    let target_duration = target_time.duration_since(sample1.time).as_secs_f64();

    // Determine the interpolation factor (t), typically between 0.0 and 1.0
    // (Note: This naturally handles extrapolation if target_time is outside the bounds)
    let t = target_duration / total_duration;

    // Convert positions from nanoseconds to seconds
    let pos1_secs = sample1.record_pos as f64 / 1_000_000_000.0;
    let pos2_secs = sample2.record_pos as f64 / 1_000_000_000.0;

    // Perform standard linear interpolation: lerp(a, b, t) = a + t * (b - a)
    let interpolated_secs = pos1_secs + t * (pos2_secs - pos1_secs);

    interpolated_secs
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use super::*;

    /// Helper to generate deterministic timestamps
    pub fn base_time(secs: u64) -> Instant {
        static FAKE_TIME: OnceLock<Instant> = OnceLock::new();
        *FAKE_TIME.get_or_init(Instant::now) + Duration::from_secs(secs)
    }

    #[test]
    fn test_empty_buffer() {
        let (_prod, cons) = RingBuffer::<PlatterSample>::new(4);
        let mut platter = VirtualPlatter::_new(cons);

        let target_time = base_time(5);
        let result = platter.get_sample(target_time);
        assert_eq!(result, Err(PlatterError::NoSamples));
    }

    #[test]
    fn test_single_sample_buffer() {
        let (mut prod, cons) = RingBuffer::<PlatterSample>::new(4);
        let mut platter = VirtualPlatter::_new(cons);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 100_000,
        };
        prod.push(s1).unwrap();

        // Case A: target time is older than the single sample
        assert_eq!(
            platter.get_sample(base_time(5)),
            Err(PlatterError::OldSampleRequested { oldest: s1 })
        );

        // NOTE: No re-push needed! platter.sample0 cached s1 safely
        // even though it was popped from the underlying rtrb ring buffer.

        // Case B: target time is newer than the single sample
        assert_eq!(
            platter.get_sample(base_time(15)),
            Err(PlatterError::NewerSampleRequested { newest: s1 })
        );
    }

    #[test]
    fn test_target_older_than_all_samples() {
        let (mut prod, cons) = RingBuffer::<PlatterSample>::new(4);
        let mut platter = VirtualPlatter::_new(cons);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 100_000,
        };
        let s2 = PlatterSample {
            time: base_time(20),
            record_pos: 200_000,
        };
        prod.push(s1).unwrap();
        prod.push(s2).unwrap();

        let result = platter.get_sample(base_time(5));
        assert_eq!(result, Err(PlatterError::OldSampleRequested { oldest: s1 }));
    }

    #[test]
    fn test_exact_bracket_match_immediate() {
        let (mut prod, cons) = RingBuffer::<PlatterSample>::new(4);
        let mut platter = VirtualPlatter::_new(cons);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 100_000,
        };
        let s2 = PlatterSample {
            time: base_time(20),
            record_pos: 200_000,
        };
        prod.push(s1).unwrap();
        prod.push(s2).unwrap();

        let result = platter.get_sample(base_time(15));
        assert_eq!(result, Ok((s1, s2)));
    }

    #[test]
    fn test_bracket_match_after_looping() {
        let (mut prod, cons) = RingBuffer::<PlatterSample>::new(4);
        let mut platter = VirtualPlatter::_new(cons);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 100_000,
        };
        let s2 = PlatterSample {
            time: base_time(20),
            record_pos: 200_000,
        };
        let s3 = PlatterSample {
            time: base_time(30),
            record_pos: 300_000,
        };
        let s4 = PlatterSample {
            time: base_time(40),
            record_pos: 400_000,
        };

        prod.push(s1).unwrap();
        prod.push(s2).unwrap();
        prod.push(s3).unwrap();
        prod.push(s4).unwrap();

        let result = platter.get_sample(base_time(25));
        assert_eq!(result, Ok((s2, s3)));
    }

    #[test]
    fn test_exact_on_boundary_match() {
        let (mut prod, cons) = RingBuffer::<PlatterSample>::new(4);
        let mut platter = VirtualPlatter::_new(cons);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 100_000,
        };
        let s2 = PlatterSample {
            time: base_time(20),
            record_pos: 200_000,
        };
        let s3 = PlatterSample {
            time: base_time(30),
            record_pos: 300_000,
        };

        prod.push(s1).unwrap();
        prod.push(s2).unwrap();
        prod.push(s3).unwrap();

        // Exact match on s2 time (20).
        // Your peek code pops s1, sets s0 = s2, peeks s3, returns (s2, s3)
        let result = platter.get_sample(base_time(20));
        assert_eq!(result, Ok((s2, s3)));
    }

    #[test]
    fn test_target_newer_than_all_samples() {
        let (mut prod, cons) = RingBuffer::<PlatterSample>::new(4);
        let mut platter = VirtualPlatter::_new(cons);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 100_000,
        };
        let s2 = PlatterSample {
            time: base_time(20),
            record_pos: 200_000,
        };
        prod.push(s1).unwrap();
        prod.push(s2).unwrap();

        let result = platter.get_sample(base_time(30));
        assert_eq!(
            result,
            Err(PlatterError::NewerSampleRequested { newest: s2 })
        );
    }

    #[test]
    fn test_successive_monotonic_calls() {
        let (mut prod, cons) = RingBuffer::<PlatterSample>::new(8);
        let mut platter = VirtualPlatter::_new(cons);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 10,
        };
        let s2 = PlatterSample {
            time: base_time(20),
            record_pos: 20,
        };
        let s3 = PlatterSample {
            time: base_time(30),
            record_pos: 30,
        };
        let s4 = PlatterSample {
            time: base_time(40),
            record_pos: 40,
        };
        let s5 = PlatterSample {
            time: base_time(50),
            record_pos: 50,
        };

        prod.push(s1).unwrap();
        prod.push(s2).unwrap();
        prod.push(s3).unwrap();
        prod.push(s4).unwrap();
        prod.push(s5).unwrap();

        // Stream tick 1 (Time = 22ms) -> Bounded by s2 (20) and s3 (30)
        let result1 = platter.get_sample(base_time(22));
        assert_eq!(result1, Ok((s2, s3)));

        // Stream tick 2 (Time = 40ms) -> Bounded by s4 (40) and s5 (50)
        let result2 = platter.get_sample(base_time(40));
        assert_eq!(result2, Ok((s4, s5)));
    }

    #[test]
    fn test_time_stall_duplicate_returns() {
        let base_time = Instant::now();
        let (mut producer, consumer) = rtrb::RingBuffer::<PlatterSample>::new(4);

        let s0_init = PlatterSample {
            time: base_time,
            record_pos: 0,
        };
        let s1 = PlatterSample {
            time: base_time + Duration::from_millis(10),
            record_pos: 100,
        };
        let s2 = PlatterSample {
            time: base_time + Duration::from_millis(20),
            record_pos: 200,
        };

        producer.push(s1).unwrap();
        producer.push(s2).unwrap();

        let mut platter = VirtualPlatter {
            sample0: Some(s0_init),
            samples: consumer,
        };

        // First call at 5ms (between s0_init at 0ms and s1 at 10ms)
        let res1 = platter
            .get_sample(base_time + Duration::from_millis(5))
            .unwrap();
        assert_eq!(res1.0.record_pos, 0);
        assert_eq!(res1.1.record_pos, 100);

        // CRITICAL SECOND CALL: Time advances slightly to 6ms.
        let res2 = platter
            .get_sample(base_time + Duration::from_millis(6))
            .unwrap();

        assert_eq!(
            res2.0.record_pos, 0,
            "Expected past anchor to stay 0, but it advanced prematurely to {}",
            res2.0.record_pos
        );
    }
}
