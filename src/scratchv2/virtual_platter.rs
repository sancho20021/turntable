use std::{sync::Arc, time::Instant};

use crossbeam::atomic::AtomicCell;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PlatterSample {
    /// When the sample was recorded
    pub timestamp_nanos: u64,
    /// Record position in nanoseconds from the start
    pub record_pos: i64,
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
                timestamp_nanos: 0,
                record_pos: 0,
            })),
            base_time,
        }
    }

    /// timestamp of Instant::now relative to base_time in nanos
    pub fn now(&self) -> u64 {
        (Instant::now() - self.base_time).as_nanos() as u64
    }

    /// Retrieves current playhead position
    pub fn get_playhead(&self) -> PlatterSample {
        self.playhead.load()
    }

    /// Updates current playhead position
    pub fn update_playhead(&self, pos_nanos: i64, timestamp_nanos: u64) {
        self.playhead.store(PlatterSample {
            timestamp_nanos,
            record_pos: pos_nanos,
        });
    }
}
