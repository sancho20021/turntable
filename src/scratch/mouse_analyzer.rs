use std::time::{Duration, Instant};

pub struct MouseSample {
    pub timestamp: Duration,
    pub x: f64,
}

pub struct MovementRecorder {
    start_time: Instant,
    samples: Vec<Vec<MouseSample>>,
}

impl MovementRecorder {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            // Pre-allocate 10 seconds of data at 125Hz to avoid re-allocations
            samples: vec![vec![]],
        }
    }

    /// Record a point. Call this inside your MouseMotion match arm.
    pub fn record(&mut self, x: f64) {
        self.samples.last_mut().unwrap().push(MouseSample {
            timestamp: self.start_time.elapsed(),
            x,
        });
    }

    pub fn finish_motion(&mut self) {
        self.samples.push(vec![]);
    }

    /// A simple "Smoothness Report"
    pub fn analyze(&self) {
    println!("\n{:=^60}", " VELOCITY STABILITY ANALYSIS ");
    println!("{:<8} | {:<10} | {:<12} | {:<12}", "Burst", "Avg Vel", "V-Jitter", "Consistency");
    println!("{}", "-".repeat(60));

    for (idx, burst) in self.samples.iter().enumerate() {
        if burst.len() < 3 { continue; }

        let mut velocities = Vec::new();

        for i in 1..burst.len() {
            let dx = (burst[i].x - burst[i-1].x).abs() as f64;
            let dt = (burst[i].timestamp - burst[i-1].timestamp).as_secs_f64();

            if dt > 0.0 {
                // Velocity in Pixels per Second
                velocities.push(dx / dt);
            }
        }

        let avg_vel = velocities.iter().sum::<f64>() / velocities.len() as f64;

        // V-Jitter: Standard deviation of velocity
        // High V-Jitter means the "speed" is jumping around even if your hand is steady
        let variance = velocities.iter()
            .map(|&v| (v - avg_vel).powi(2))
            .sum::<f64>() / velocities.len() as f64;
        let v_jitter = variance.sqrt();

        // Consistency: How much of the speed is "noise"
        // (1.0 - Relative Standard Deviation)
        let consistency = if avg_vel > 0.0 {
            (1.0 - (v_jitter / avg_vel)).max(0.0) * 100.0
        } else {
            0.0
        };

        println!(
            "#{:<7} | {:>8.1} px/s | {:>9.1} | {:>10.1}%",
            idx + 1, avg_vel, v_jitter, consistency
        );
    }
}
}
