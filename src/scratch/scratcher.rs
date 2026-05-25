use std::sync::{Arc, atomic::Ordering};

use crate::scratch::{record::Record, shared_state::ScratchState};

/// Scratcher. Interprets scratch state and generates sound frames
pub struct Scratcher<R> {
    record: R,
    state: Arc<ScratchState>,
    sensitivity: f64,
}

#[derive(Debug, Clone)]
pub struct ScratchStateSnapshot {
    pub is_scratching: bool,
    pub mouse_x: i32,
    pub anchor_sample: f64,
    pub anchor_x: i32,
    pub playhead: f64,
    pub speed: f64,
}

impl<R> Scratcher<R> {
    pub fn new(record: R, state: Arc<ScratchState>, sensitivity: f64) -> Self {
        Self {
            record,
            state,
            sensitivity,
        }
    }
}

impl<R: Record> Scratcher<R> {
    fn load_state(&self) -> ScratchStateSnapshot {
        let state = &self.state;
        // 1. SNAP: Read atomics once per buffer
        let is_scratching = state.scratching.load(Ordering::Relaxed);
        let mouse_x = state.current_x.load(Ordering::Relaxed);
        let anchor_sample = state.anchor_sample.load(Ordering::Relaxed);
        let anchor_x = state.anchor_x.load(Ordering::Relaxed);
        let playhead = state.playhead.load(Ordering::Relaxed);
        let speed = state.speed.load(Ordering::Relaxed);
        ScratchStateSnapshot {
            is_scratching,
            mouse_x,
            anchor_sample,
            anchor_x,
            playhead,
            speed,
        }
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&self, data: &mut [f32]) {
        let snapshot = self.load_state();
        let mut playhead = snapshot.playhead;

        // 2. PRE-CALCULATE: Figure out the "Target" for the end of this buffer
        let target_playhead = if snapshot.is_scratching {
            snapshot.anchor_sample
                + ((snapshot.mouse_x - snapshot.anchor_x) as f64) * self.sensitivity
        } else {
            playhead + data.len() as f64 * snapshot.speed / 2.
        };

        let step = (target_playhead - playhead) / (data.len() as f64 / 2.);

        // 3. LOOP: Process the samples
        for frame in data.chunks_mut(2) {
            // Calculate current sample based on playhead
            let s = self.record.get_sample(playhead);

            playhead += step;

            frame[0] = s.l;
            frame[1] = s.r;
        }
        // Update the playhead
        self.state.playhead.store(playhead, Ordering::Relaxed);
    }
}
