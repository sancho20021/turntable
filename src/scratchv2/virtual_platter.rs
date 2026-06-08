use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PlatterSample {
    /// When the sample was recorded, converted to audio thread clock
    pub time: cpal::StreamInstant,
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
    when: cpal::StreamInstant,
    samples: &mut Consumer<PlatterSample>,
) -> Result<(PlatterSample, PlatterSample), PlatterError> {
    let mut sample0 = if let Ok(sample) = samples.pop() {
        sample
    } else {
        return Err(PlatterError::NoSamples);
    };

    if when < sample0.time {
        return Err(PlatterError::OldSampleRequested { oldest: sample0 });
    }

    // sample0 <= when

    let mut sample1 = if let Ok(sample) = samples.pop() {
        sample
    } else {
        return Err(PlatterError::NewerSampleRequested { newest: sample0 });
    };

    while sample1.time <= when {
        if let Ok(sample) = samples.pop() {
            sample0 = sample1;
            sample1 = sample;
        } else {
            return Err(PlatterError::NewerSampleRequested { newest: sample1 });
        }
    }
    // sample0 <= when < sample1
    Ok((sample0, sample1))
}

/// Initializes a concurrent wait and lock free ring buffer
/// to represent a virtual platter that is to be read by an audio thread
/// and updated by some virtual platter logic
///
/// - `audio_block_duration`: The exact time span of one audio block (e.g., 512 samples @ 44.1kHz ≈ 11.6ms).
/// - `update_frequency_hz`: How many times per second the producer spins/spams position updates.
/// - `jitter_factor`: Safety multiplier to handle OS scheduling noise (2.0 to 3.0 recommended).
pub fn new_platter_buffer(
    audio_block_duration: Duration,
    update_frequency_hz: f64,
    jitter_factor: f64,
) -> (Producer<PlatterSample>, Consumer<PlatterSample>) {
    // 1. Convert duration to seconds
    let block_duration_secs = audio_block_duration.as_secs_f64();

    // 2. Calculate ideal slots: Duration * Frequency * Jitter
    let ideal_capacity = block_duration_secs * update_frequency_hz * jitter_factor;

    // 3. Round up to the nearest integer and enforce a reasonable minimum floor (e.g., 4 slots)
    let calculated_slots = (ideal_capacity.ceil() as usize).max(4);

    // 4. Round up to the next power of two.
    // While `rtrb` works with any size, power-of-two sizes optimize hardware ring-masking
    // performance and memory alignment.
    let final_capacity = calculated_slots.next_power_of_two();

    // 5. Instantiate the lock-free ring buffer
    RingBuffer::new(final_capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to generate deterministic timestamps
    fn base_time(secs: u64) -> cpal::StreamInstant {
        cpal::StreamInstant::new(secs as i64, 0)
    }

    #[test]
    fn test_empty_buffer() {
        let (mut _prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(4);

        let target_time = base_time(5);
        let result = get_sample(target_time, &mut cons);
        assert_eq!(result, Err(PlatterError::NoSamples));
    }

    #[test]
    fn test_single_sample_buffer() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(4);

        let s1 = PlatterSample {
            time: base_time(10),
            record_pos: 100_000,
        };
        prod.push(s1).unwrap();

        // Case A: target time is older than the single sample
        assert_eq!(
            get_sample(base_time(5), &mut cons),
            Err(PlatterError::OldSampleRequested { oldest: s1 })
        );

        // Re-push since the previous execution consumed it
        prod.push(s1).unwrap();

        // Case B: target time is newer than the single sample
        assert_eq!(
            get_sample(base_time(15), &mut cons),
            Err(PlatterError::NewerSampleRequested { newest: s1 })
        );
    }

    #[test]
    fn test_target_older_than_all_samples() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(4);

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

        let result = get_sample(base_time(5), &mut cons);
        assert_eq!(result, Err(PlatterError::OldSampleRequested { oldest: s1 }));
    }

    #[test]
    fn test_exact_bracket_match_immediate() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(4);

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

        let result = get_sample(base_time(15), &mut cons);
        assert_eq!(result, Ok((s1, s2)));
    }

    #[test]
    fn test_bracket_match_after_looping() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(4);

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

        let result = get_sample(base_time(25), &mut cons);
        assert_eq!(result, Ok((s2, s3)));
    }

    #[test]
    fn test_exact_on_boundary_match() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(4);

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

        let result = get_sample(base_time(20), &mut cons);
        assert_eq!(result, Ok((s2, s3)));
    }

    #[test]
    fn test_target_newer_than_all_samples() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(4);

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

        let result = get_sample(base_time(30), &mut cons);
        assert_eq!(
            result,
            Err(PlatterError::NewerSampleRequested { newest: s2 })
        );
    }

    #[test]
    fn test_successive_monotonic_calls() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<PlatterSample>::new(8);

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

        // Stream tick 1
        let result1 = get_sample(base_time(22), &mut cons);
        assert_eq!(result1, Ok((s2, s3)));

        // Stream tick 2 (picks up exactly where the last one left off)
        let result2 = get_sample(base_time(40), &mut cons);
        assert_eq!(result2, Ok((s4, s5)));
    }
}
