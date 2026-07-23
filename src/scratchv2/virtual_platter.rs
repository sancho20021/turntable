use std::{sync::Arc, time::Instant};

use crossbeam::atomic::AtomicCell;

#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub struct UNanos(pub u64);

#[derive(Debug, PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub struct INanos(pub i64);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PlatterSample {
    /// When the sample was recorded
    pub timestamp_nanos: UNanos,
    /// Record position in nanoseconds from the start
    pub record_pos: INanos,
}

#[derive(Debug, Clone)]
pub struct VirtualPlatter {
    playhead: Arc<AtomicCell<PlatterSample>>,
    base_time: Instant,
}

impl VirtualPlatter {
    pub fn new() -> Self {
        let base_time = Instant::now();
        Self {
            playhead: Arc::new(AtomicCell::new(PlatterSample {
                timestamp_nanos: UNanos(0),
                record_pos: INanos(0),
            })),
            base_time,
        }
    }

    /// timestamp of Instant::now relative to base_time in nanos
    pub fn now(&self) -> UNanos {
        UNanos((Instant::now() - self.base_time).as_nanos() as u64)
    }

    /// Retrieves current playhead position
    pub fn get_playhead(&self) -> PlatterSample {
        self.playhead.load()
    }

    /// Updates current playhead position
    pub fn update_playhead(&self, pos_nanos: INanos, timestamp_nanos: UNanos) {
        self.playhead.store(PlatterSample {
            timestamp_nanos,
            record_pos: pos_nanos,
        });
    }
}
