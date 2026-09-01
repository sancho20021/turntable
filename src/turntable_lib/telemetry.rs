//! To record events with timestamps and later visualize and analyze them in python etc

use std::fs::File;
use std::io::{BufWriter, Write};

use crate::record::UNanos;

#[derive(Debug, Clone)]
pub struct TelemetrySample {
    pub timestamp_micros: u64,
    pub metric_name: String,
    pub value: f64,
}

#[derive(Debug)]
pub struct TelemetryTrace {
    pub samples: Vec<TelemetrySample>,
}

impl TelemetryTrace {
    pub fn new() -> Self {
        Self {
            samples: Vec::with_capacity(10_000),
        }
    }

    /// Record a math metric snapshot in time
    #[inline]
    pub fn record<S: AsRef<str>>(&mut self, timestamp: UNanos, metric_name: S, value: f64) {
        self.samples.push(TelemetrySample {
            timestamp_micros: timestamp.as_micros(),
            metric_name: metric_name.as_ref().to_string(),
            value,
        });
    }

    /// Explicitly append data from another tracker loop
    pub fn append(&mut self, other: &mut TelemetryTrace) {
        self.samples.append(&mut other.samples);
    }

    /// Convenience Method: Drains an entire iterator of telemetry traces,
    /// sorts everything chronologically, and saves the file out.
    pub fn join_all<I>(&mut self, traces: I, path: &str) -> std::io::Result<()>
    where
        I: IntoIterator<Item = TelemetryTrace>,
    {
        for mut trace in traces {
            self.samples.append(&mut trace.samples);
        }
        self.save_to_file(path)
    }

    /// Does not create a file if collected samples are empty
    pub fn save_to_file(&mut self, path: &str) -> std::io::Result<()> {
        if self.samples.is_empty() {
            return Ok(());
        }
        self.samples.sort_unstable_by_key(|s| s.timestamp_micros);

        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);

        // Header matching your exact Jupyter script needs
        writeln!(writer, "timestamp_us,metric,value")?;
        for sample in &self.samples {
            writeln!(
                writer,
                "{},{},{}",
                sample.timestamp_micros, sample.metric_name, sample.value
            )?;
        }

        writer.flush()?;
        log::info!(
            "Exported {} telemetry points to {}!",
            self.samples.len(),
            path
        );
        Ok(())
    }
}

/// Standalone helper to combine thread outputs immediately into a file
pub fn save_traces_to_file<I>(traces: I, path: &str) -> std::io::Result<()>
where
    I: IntoIterator<Item = TelemetryTrace>,
{
    let mut master = TelemetryTrace::new();
    master.join_all(traces, path)
}

#[macro_export]
macro_rules! record_input {
    // If the feature flag IS active, expand to the actual function call
    ($trace:expr, $timestamp:expr, $metric:expr, $value:expr) => {
        #[cfg(feature = "trace-input")]
        {
            $trace.record($timestamp, $metric, $value);
        }
    };
}
