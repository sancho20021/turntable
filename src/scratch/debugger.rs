use std::{
    sync::{Arc, atomic::Ordering},
    thread,
    time::Duration,
};

use crate::scratch::shared_state::ScratchState;

pub fn monitor_scratch_state(controller: Arc<ScratchState>) {
    thread::spawn(move || {
        loop {
            // Loading with Relaxed ordering is sufficient for simple logging/telemetry
            let playhead = controller.playhead.load(Ordering::Relaxed);
            let scratching = controller.scratching.load(Ordering::Relaxed);
            let anchor_sample = controller.anchor_sample.load(Ordering::Relaxed);
            let anchor_x = controller.anchor_x.load(Ordering::Relaxed);
            let current_x = controller.current_x.load(Ordering::Relaxed);

            println!("--- Scratch State ---");
            println!(
                "Status:     {}",
                if scratching { "SCRATCHING" } else { "PLAYING" }
            );
            println!("Playhead:   {:.2}", playhead);
            println!(
                "Anchor:     {:.2} (Sample) | {} (X)",
                anchor_sample, anchor_x
            );
            println!("Current X:  {}", current_x);
            println!("Delta X:    {}", current_x - anchor_x);
            println!("---------------------\n");

            // Wait for 1 second before the next print
            thread::sleep(Duration::from_secs(1));
        }
    });
}
