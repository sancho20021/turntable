use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub struct MouseSample {
    pub timestamp: Duration,
    pub raw_x: f64,
}

pub struct MovementRecorder {
    samples: Vec<MouseSample>,
    pub start: Instant,
    output: PathBuf,
}

impl MovementRecorder {
    pub fn new(out: &Path) -> Self {
        Self {
            samples: Vec::with_capacity(1000),
            start: Instant::now(),
            output: out.to_path_buf(),
        }
    }

    /// Record a point. Call this inside your MouseMotion match arm.
    pub fn record(&mut self, x: f64) {
        self.samples.push(MouseSample {
            timestamp: self.start.elapsed(),
            raw_x: x,
        });
    }

    /// Flattens the burst data and saves it as a CSV file for Python analysis
    pub fn finish(&self) {
        println!("\n{:=^60}", " EXPORTING TELEMETRY DATA ");

        // 1. Build the single giant string in memory
        let mut csv_output = String::from("timestamp_ms,x\n");

        // Map your flat samples directly to formatted strings
        let rows: Vec<String> = self
            .samples
            .iter()
            .map(|s| format!("{},{}\n", s.timestamp.as_millis(), s.raw_x))
            .collect();

        csv_output.push_str(&rows.concat());

        // 2. Write everything out to the file path in one shot
        match std::fs::write(&self.output, csv_output) {
            Ok(_) => {
                println!(
                    "Successfully wrote {} points to: {:?}",
                    self.samples.len(),
                    self.output
                );
                println!("{:=^60}\n", "");
            }
            Err(e) => println!("Failed to save telemetry file: {}", e),
        }
    }
}
