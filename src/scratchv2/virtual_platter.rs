use std::{marker::PhantomData, sync::Arc, time::Instant};

use crossbeam::atomic::AtomicCell;

use crate::record::{INanos, UNanos};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PlatterSample {
    /// When the sample was recorded
    pub timestamp_nanos: UNanos,
    /// Record position in nanoseconds from the start
    pub record_pos: INanos,
}

#[derive(Debug)]
pub struct Write;
#[derive(Debug)]
pub struct Read;

#[derive(Debug)]
pub struct VirtualPlatter<Mode> {
    playhead: Arc<AtomicCell<PlatterSample>>,
    base_time: Instant,
    _mode: PhantomData<Mode>,
}

pub type ReadablePlatter = VirtualPlatter<Read>;
pub type WritablePlatter = VirtualPlatter<Write>;

impl Clone for ReadablePlatter {
    fn clone(&self) -> Self {
        Self {
            playhead: self.playhead.clone(),
            base_time: self.base_time.clone(),
            _mode: self._mode.clone(),
        }
    }
}

pub fn new_platter() -> (VirtualPlatter<Write>, VirtualPlatter<Read>) {
    let base_time = Instant::now();
    let playhead = Arc::new(AtomicCell::new(PlatterSample {
        timestamp_nanos: UNanos(0),
        record_pos: INanos(0),
    }));
    let write = VirtualPlatter {
        playhead: Arc::clone(&playhead),
        base_time,
        _mode: PhantomData,
    };
    let read = VirtualPlatter {
        playhead,
        base_time,
        _mode: PhantomData,
    };
    (write, read)
}

impl<AnyMode> VirtualPlatter<AnyMode> {
    /// timestamp of Instant::now relative to base_time in nanos
    pub fn now(&self) -> UNanos {
        UNanos((Instant::now() - self.base_time).as_nanos() as u64)
    }

    /// Retrieves current playhead position
    pub fn get_playhead(&self) -> PlatterSample {
        self.playhead.load()
    }

    pub fn timestamp(&self, timestamp: Instant) -> UNanos {
        UNanos((timestamp - self.base_time).as_nanos() as u64)
    }
}

impl VirtualPlatter<Write> {
    /// Updates current playhead position
    pub fn update_playhead(&mut self, pos_nanos: INanos, timestamp_nanos: UNanos) {
        self.playhead.store(PlatterSample {
            timestamp_nanos,
            record_pos: pos_nanos,
        });
    }
}
