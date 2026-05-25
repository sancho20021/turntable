use std::sync::{Arc, atomic::Ordering};

use crate::scratch::{record::Record, shared_state::ScratchState};

/// Scratcher. Interprets scratch state and generates sound frames
pub struct Scratcher<R> {
    record: R,
    state: Arc<ScratchState>,
    sensitivity: f64,
    // --- PERSISTENT INERTIA STATE ---
    virtual_x: f64, // Tracked virtual mouse position
    virtual_v: f64, // Internal tracking velocity (pixels/second)
    last_time: std::time::Instant,
    omega: f64,
}

#[derive(Debug, Clone)]
pub struct ScratchStateSnapshot {
    pub is_scratching: bool,
    pub mouse_x: f64,
    pub anchor_sample: f64,
    pub anchor_x: f64,
    pub playhead: f64,
    pub speed: f64,
}

impl<R> Scratcher<R> {
    /// Omega - inertia parameter for smoothing the mouse movement inbetween mouse events
    /// 40.0 to 60.0 provides an incredibly responsive, organic without being mushy. Tune this to your taste!
    pub fn new(record: R, state: Arc<ScratchState>, sensitivity: f64, omega: f64) -> Self {
        Self {
            record,
            state,
            sensitivity,
            virtual_x: 0.0,
            virtual_v: 0.0,
            last_time: std::time::Instant::now(),
            omega,
        }
    }
}

impl<R: Record> Scratcher<R> {
    fn load_state(&self) -> ScratchStateSnapshot {
        let state = &self.state;
        // 1. SNAP: Read atomics once per buffer
        let is_scratching = state.scratching.load(Ordering::Relaxed);
        let mouse_x = state.current_x.load(Ordering::Relaxed) as f64;
        let anchor_sample = state.anchor_sample.load(Ordering::Relaxed);
        let anchor_x = state.anchor_x.load(Ordering::Relaxed) as f64;
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

    fn filter_state(&mut self, snapshot: ScratchStateSnapshot) -> ScratchStateSnapshot {
        // --- TIME DELTA CALCULATION ---
        let now = std::time::Instant::now();
        let dt = (now - self.last_time).as_secs_f64();
        self.last_time = now;

        let smoothed_x = if snapshot.is_scratching {
            // Guard clause for safety or initialization drops
            if dt > 0.0 {
                // Analytical solution to a critically damped spring-mass system
                let target_x = snapshot.mouse_x;
                let a = self.virtual_x - target_x;
                let b = self.virtual_v + self.omega * a;

                let exp_term = (-self.omega * dt).exp();

                // Update internal position and velocity trackers
                self.virtual_x = (a + b * dt) * exp_term + target_x;
                self.virtual_v = (b - self.omega * (a + b * dt)) * exp_term;
            }
            self.virtual_x
        } else {
            // Hard reset physical tracking states when user drops mouse handle
            self.virtual_x = snapshot.mouse_x;
            self.virtual_v = 0.0;
            snapshot.mouse_x
        };

        ScratchStateSnapshot {
            mouse_x: smoothed_x,
            ..snapshot
        }
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, data: &mut [f32]) {
        let snapshot = self.load_state();
        let snapshot = self.filter_state(snapshot);

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
