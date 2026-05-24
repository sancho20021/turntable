use std::sync::atomic::Ordering;

use crate::{
    deck::interpolator::Interpolator,
    scratch::{controller::ScratchController, record::Record},
};

/// Warning: this function must be very fast. Can't to any allocation
pub fn write_frames<Int: Interpolator>(
    data: &mut [f32],
    record: &Record<Int>,
    controller: &ScratchController,
) {
    // 1. SNAP: Read atomics once per buffer
    let is_scratching = controller.scratching.load(Ordering::Relaxed);
    let mouse_x = controller.current_x.load(Ordering::Relaxed);
    let anchor_sample = controller.anchor_sample.load(Ordering::Relaxed);
    let anchor_x = controller.anchor_x.load(Ordering::Relaxed);
    let mut playhead = controller.playhead.load(Ordering::Relaxed);

    // 2. PRE-CALCULATE: Figure out the "Target" for the end of this buffer
    let target_playhead = if is_scratching {
        anchor_sample + ((mouse_x - anchor_x) as f64) * controller.sensitivity
    } else {
        playhead + data.len() as f64 / 2.
    };

    let step = (target_playhead - playhead) / (data.len() as f64 / 2.);

    // 3. LOOP: Process the samples
    for frame in data.chunks_mut(2) {
        // Calculate current sample based on playhead
        let s = record.get_sample(playhead);

        // Advance playhead slightly each iteration to reach target
        // (This prevents "zipper noise" if the mouse moved a lot)
        // playhead += (target_playhead - playhead) * 0.1; // Simple smoothing

        playhead += step;

        frame[0] = s.l;
        frame[1] = s.r;
    }
    // Update the playhead
    controller.playhead.store(playhead, Ordering::Relaxed);
}
