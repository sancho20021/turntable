use std::{
    path::{Path, PathBuf},
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
    state: Arc<ScratchState>,
    output: PathBuf,
    shutdown_flag: Arc<AtomicBool>,
}

impl DeckMonitor {
    pub fn new(state: Arc<ScratchState>, output: &Path) -> Self {
        Self {
            output: output.to_path_buf(),
            state,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(&self, start_time: Instant) {
        let mut samples = Vec::with_capacity(10_000);
        let output = self.output.clone();
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let state = Arc::clone(&self.state);

        std::thread::spawn(move || {
            println!("[Monitor] Background target tracking active...");

            while !shutdown_flag.load(Ordering::Relaxed) {
                match state.platter.load() {
                    crate::scratch::shared_state::PlatterState::Playing => (),
                    crate::scratch::shared_state::PlatterState::Scratching { .. } => {
                        let timestamp = start_time.elapsed();
                        let playhead = state.playhead.load(Ordering::Relaxed);
                        samples.push(DeckSample {
                            timestamp,
                            playhead,
                        });
                    }
                }

                // High frequency polling rate (approx 1000Hz)
                std::thread::sleep(Duration::from_millis(4));
            }

            // --- EXPORT PHASE (Triggers on exit) ---
            println!("\n{:=^60}", " EXPORTING MONITOR TELEMETRY DATA ");

            let mut csv_output = String::from("timestamp_ms,playhead\n");

            let rows: Vec<String> = samples
                .iter()
                .map(|s| format!("{},{:.4}\n", s.timestamp.as_millis(), s.playhead,))
                .collect();

            csv_output.push_str(&rows.concat());

            // Write everything out to the file path in one shot
            match std::fs::write(&output, csv_output) {
                Ok(_) => {
                    println!(
                        "Successfully wrote {} points to: {:?}",
                        samples.len(),
                        output
                    );
                    println!("{:=^60}\n", "");
                }
                Err(e) => println!("Failed to save telemetry file: {}", e),
            }
        });
    }

    pub fn finish(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(50));
    }
}
