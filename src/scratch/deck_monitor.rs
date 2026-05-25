use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::scratch::shared_state::ScratchState;

pub struct DeckSample {
    pub timestamp: Duration,
    pub playhead: f64,
}

pub struct DeckMonitor {
    samples: Vec<DeckSample>,
    start: Instant,
    output: PathBuf,
}

pub fn monitor_deck_playhead(
    state: Arc<ScratchState>,
    shutdown_flag: Arc<AtomicBool>,
    output_path: PathBuf,
) {
    let mut monitor = DeckMonitor {
        samples: Vec::with_capacity(10_000),
        start: Instant::now(),
        output: output_path,
    };
    std::thread::spawn(move || {
        println!("[Monitor] Background target tracking active...");

        while !shutdown_flag.load(Ordering::Relaxed) {
            let is_scratching = state.scratching.load(Ordering::Relaxed);

            if is_scratching {
                let timestamp = monitor.start.elapsed();
                let playhead = state.playhead.load(Ordering::Relaxed);
                monitor.samples.push(DeckSample {
                    timestamp,
                    playhead,
                });
            }

            // High frequency polling rate (approx 1000Hz)
            std::thread::sleep(Duration::from_millis(1));
        }

        // --- EXPORT PHASE (Triggers on exit) ---
        println!("\n{:=^60}", " EXPORTING MONITOR TELEMETRY DATA ");

        let mut csv_output = String::from("timestamp_ms,playhead\n");

        let rows: Vec<String> = monitor
            .samples
            .iter()
            .map(|s| format!("{},{}\n", s.timestamp.as_millis(), s.playhead))
            .collect();

        csv_output.push_str(&rows.concat());

        // Write everything out to the file path in one shot
        match std::fs::write(&monitor.output, csv_output) {
            Ok(_) => {
                println!(
                    "Successfully wrote {} points to: {:?}",
                    monitor.samples.len(),
                    monitor.output
                );
                println!("{:=^60}\n", "");
            }
            Err(e) => println!("Failed to save telemetry file: {}", e),
        }
    });
}
