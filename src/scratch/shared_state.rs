use atomic_float::AtomicF64;
use crossbeam::atomic::AtomicCell;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub enum PlatterState {
    Playing,
    Scratching {
        anchor_sample: f64,
        anchor_mouse: i32,
        start_time: Instant,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct MouseUpdate {
    pub pos: f64,
    pub when: Instant,
}

/// Communication channel between touchpad and scratching engine
pub struct ScratchState {
    /// virtual playhead. Current sample
    pub playhead: AtomicF64,
    /// whether we are just playing or scratching
    // TODO: make it lock-free by reducing the size of platterstate
    pub platter: AtomicCell<PlatterState>,
    /// current touchpad / platter position
    pub latest_mouse: AtomicCell<MouseUpdate>,
    /// playback speed
    pub speed: AtomicF64,
}

impl ScratchState {
    pub fn new(playhead: f64) -> Self {
        Self {
            playhead: AtomicF64::new(playhead),
            platter: AtomicCell::new(PlatterState::Playing),
            latest_mouse: AtomicCell::new(MouseUpdate {
                pos: 0.,
                when: Instant::now(),
            }),
            speed: AtomicF64::new(1.),
        }
    }
}
