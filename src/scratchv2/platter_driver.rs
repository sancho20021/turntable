use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::scratchv2::scratch_controller::ScratchController;

pub fn spawn_platter_driver(
    controller: ScratchController,
    update_frequency_hz: f64,
    shutdown_flag: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let interval = Duration::from_secs_f64(1.0 / update_frequency_hz);

        while !shutdown_flag.load(Ordering::Relaxed) {
            let loop_start = Instant::now();
            controller.update_platter();
            // 5. High-precision sleep to maintain targeted update frequency
            let elapsed = loop_start.elapsed();
            if elapsed < interval {
                std::thread::sleep(interval - elapsed);
            }
        }

        println!("Platter stopped");
    })
}
