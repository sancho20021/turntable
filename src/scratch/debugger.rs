use std::{
    sync::{Arc, atomic::Ordering},
    thread,
    time::Duration,
};

use crate::scratch::shared_state::{PlatterState, ScratchState};

pub fn monitor_scratch_state(controller: Arc<ScratchState>) {
    thread::spawn(move || {
        loop {
            let playhead = controller.playhead.load(Ordering::Relaxed);
            let platter = controller.platter.load();
            let current_x = controller.latest_mouse.load();
            let speed = controller.speed.load(Ordering::Relaxed);

            println!("--- Scratch State ---");
            match platter {
                PlatterState::Playing => {
                    println!("PLAYING at speed {:.4}", speed);
                }
                PlatterState::Scratching {
                    anchor_sample,
                    anchor_mouse,
                    start_time: _,
                } => {
                    println!("SCRATCHING");
                    println!(
                        "Anchor:     {:.2} (Sample) | {} (X)",
                        anchor_sample, anchor_mouse
                    );
                    println!("Current X:  {}", current_x.pos);
                    println!("Delta X:    {:.2}", current_x.pos - anchor_mouse as f64);
                }
            }
            println!("Playhead:   {:.2}", playhead);
            println!("---------------------\n");

            // Wait for 1 second before the next print
            thread::sleep(Duration::from_secs(1));
        }
    });
}
