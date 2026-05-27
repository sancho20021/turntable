use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use crate::scratch::{
    record::Record,
    shared_state::{MouseUpdate, PlatterState, ScratchState},
};

/// Scratcher. Interprets scratch state and generates sound frames
pub struct Scratcher<R> {
    record: R,
    state: Arc<ScratchState>,
    sensitivity: f64,
    sample_rate: f64,
    start_time: Instant,
    latest_processed_mouse: Option<MouseUpdate>,
}

#[derive(Debug, Clone)]
pub struct ScratchStateSnapshot {
    pub playhead: f64,
    pub speed: f64,
    pub latest_mouse: MouseUpdate,
    pub platter: PlatterState,
}

impl<R> Scratcher<R> {
    pub fn new(record: R, state: Arc<ScratchState>, sensitivity: f64, sample_rate: f64) -> Self {
        Self {
            record,
            state,
            sensitivity,
            sample_rate,
            start_time: Instant::now(),
            latest_processed_mouse: None,
        }
    }
}

/// Makes audio time go behind real time
pub const LATTENCY: Duration = Duration::from_millis(20);

fn interpolate_mouse(
    mouse_was: MouseUpdate,
    mouse_will: MouseUpdate,
    current_time: Instant,
) -> f64 {
    // 1. Guard against division by zero if t0 and t1 happen to be identical instants
    if mouse_was.when == mouse_will.when {
        return mouse_was.pos as f64;
    }

    // 2. Calculate durations as f64 seconds
    let total_duration = mouse_will.when.duration_since(mouse_was.when).as_secs_f64();
    let passed_duration = current_time.duration_since(mouse_was.when).as_secs_f64();

    // 3. Get the progress fraction (clamped between 0.0 and 1.0 to prevent weird overshoot)
    // note: potentially i may increase the max to more, considering the mouse_will can be behind current_time
    let f = (passed_duration / total_duration).clamp(0.0, 1.0);

    // 4. Standard linear interpolation formula
    mouse_was.pos as f64 + f * (mouse_will.pos as f64 - mouse_was.pos as f64)
}

impl<R: Record> Scratcher<R> {
    fn load_state(&self) -> ScratchStateSnapshot {
        let state = &self.state;
        ScratchStateSnapshot {
            latest_mouse: state.latest_mouse.load(),
            playhead: state.playhead.load(Ordering::Relaxed),
            speed: state.speed.load(Ordering::Relaxed),
            platter: state.platter.load(),
        }
    }

    /// Warning: this function must be very fast, no allocation
    pub fn write_frames(&mut self, data: &mut [f32]) {
        let snapshot = self.load_state();
        let mut playhead = snapshot.playhead;

        // 2. PRE-CALCULATE: Figure out the "Target" for the end of this buffer
        let target_playhead = match snapshot.platter {
            PlatterState::Playing => {
                self.latest_processed_mouse = None;
                playhead + data.len() as f64 * snapshot.speed / 2.
            }
            PlatterState::Scratching {
                anchor_sample,
                anchor_mouse,
                start_time,
            } => {
                let latest_processed_mouse = self.latest_processed_mouse;
                if let Some(latest_processed_mouse) = latest_processed_mouse {
                    // let where_mouse_was = latest_processed_mouse.pos;
                    // let when_mouse_was = latest_processed_mouse.when;

                    // let where_mouse_now = snapshot.latest_mouse.pos;
                    // let when_mouse_now = snapshot.latest_mouse.when;

                    let when_buffer_ends = latest_processed_mouse.when
                        + Duration::from_secs_f64(data.len() as f64 / 2. / self.sample_rate);
                    let mouse_now = interpolate_mouse(
                        latest_processed_mouse,
                        snapshot.latest_mouse,
                        when_buffer_ends,
                    );
                    self.latest_processed_mouse = Some(MouseUpdate {
                        pos: mouse_now,
                        when: when_buffer_ends,
                    });

                    // println!(
                    //     "interp mouse position = {} at {}",
                    //     mouse_now,
                    //     (when_buffer_ends - self.start_time).as_millis()
                    // );

                    let target_sample = anchor_sample
                        + ((mouse_now - anchor_mouse as f64) as f64) * self.sensitivity;

                    target_sample
                } else {
                    let target_sample = anchor_sample
                        + ((snapshot.latest_mouse.pos as f64 - anchor_mouse as f64) as f64)
                            * self.sensitivity;
                    self.latest_processed_mouse = Some(MouseUpdate {
                        pos: snapshot.latest_mouse.pos,
                        when: snapshot.latest_mouse.when - LATTENCY,
                    });
                    target_sample
                }
            }
        };

        let step = (target_playhead - playhead) / (data.len() as f64 / 2.);
        // println!("step={step}");

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
