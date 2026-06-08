use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam::atomic::AtomicCell;
use rtrb::Consumer;

use crate::{
    scratch::record::Record,
    scratchv2::virtual_platter::{self, PlatterSample},
};

/// The self-contained logic unit that transforms platter ticks into audio samples.
pub struct PlatterAudioProcessor<R> {
    consumer: Consumer<PlatterSample>,
    time_bridge: TimeAnchorBridge,
    sample_rate: usize,
    record: R,
}

impl<R: Record> PlatterAudioProcessor<R> {
    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, data: &mut [f32], callback_time: cpal::StreamInstant) {
        let block_duration =
            Duration::from_secs_f64(data.len() as f64 / 2.0 / self.sample_rate as f64);

        let start = callback_time.sub(block_duration).unwrap_or(callback_time);
        let finish = callback_time;

        let start_pos = virtual_platter::get_sample(start, &mut self.consumer).unwrap();
        let finish_pos = virtual_platter::get_sample(finish, &mut self.consumer).unwrap();

        // let start = start_pos.0.record_pos
        // todo

        // 3. LOOP: Process the samples
        for frame in data.chunks_mut(2) {
            // Calculate current sample based on playhead
            todo!()
        }
    }
}

/// Structure to map `std::time::Instant` to `cpal::StreamInstant`
/// fully correcting for clock drift over time.
#[derive(Clone)]
pub struct TimeAnchorBridge {
    /// Initialized once when the bridge is created.
    system_base_time: Instant,
    /// most recent CPAL stream time anchor minus most recent CPU time anchor (in nanoseconds wince `system_base_time`)
    diff_stream_cpu_nanos: Arc<AtomicI64>,
}

impl TimeAnchorBridge {
    /// Creates a new `TimeAnchorBridge` and captures the local system base time.
    pub fn new() -> Self {
        Self {
            system_base_time: Instant::now(),
            diff_stream_cpu_nanos: Arc::new(AtomicI64::new(0)),
        }
    }

    /// **MUST be called at the very start of every single audio callback block.**
    ///
    /// This recalibrates the relationship between the CPU clock and the cpal clock,
    /// continuously resetting any clock drift before it accumulates.
    ///
    /// Pass `info.timestamp().callback` here to align the current CPU execution
    /// directly with CPAL's present timeline grid.
    pub fn update_from_audio_thread(&self, stream_now: cpal::StreamInstant) {
        let cpu_now = Instant::now();

        // cpal::StreamInstant::new(secs, nanos)

        // Calculate nanoseconds elapsed since our immutable base time
        let cpu = cpu_now.duration_since(self.system_base_time);
        let cpu_timestamp = cpal::StreamInstant::new(cpu.as_secs() as i64, cpu.subsec_nanos());

        let diff = if stream_now > cpu_timestamp {
            stream_now
                .duration_since(&cpu_timestamp)
                .unwrap()
                .as_nanos() as i64
        } else {
            -(cpu_timestamp
                .duration_since(&stream_now)
                .unwrap()
                .as_nanos() as i64)
        };
        self.diff_stream_cpu_nanos.store(diff, Ordering::Relaxed);
    }

    /// **Called by the Producer/Sensor thread to translate an event's CPU time.**
    ///
    /// Takes a standard `Instant` and projects it onto the CPAL `StreamInstant` timeline
    /// using the drift-corrected anchor established by the last audio block.
    pub fn map_to_stream_instant(&self, cpu_time: Instant) -> cpal::StreamInstant {
        let diff_stream_cpu = self.diff_stream_cpu_nanos.load(Ordering::Relaxed);

        // Calculate where this specific event falls relative to our struct's base time.
        let target_cpu_nanos = if cpu_time >= self.system_base_time {
            cpu_time.duration_since(self.system_base_time).as_nanos() as i64
        } else {
            -(self.system_base_time.duration_since(cpu_time).as_nanos() as i64)
        };

        // Calculate target nanoseconds, safely clamping to 0 before casting to u64
        let target_nanos = (target_cpu_nanos + diff_stream_cpu).max(0) as u64;
        let target_stream = Duration::from_nanos(target_nanos);

        cpal::StreamInstant::new(target_stream.as_secs() as i64, target_stream.subsec_nanos())
    }
}
