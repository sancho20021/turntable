mod app;

use std::{fs::OpenOptions, path::PathBuf};

use clap::{Parser, Subcommand};
use log::info;

use crate::app::start;

#[derive(Parser, Debug)]
#[command(author, version, about = "Turntable Scratch Engine CLI", long_about = None)]
struct Cli {
    /// Path to log file
    #[arg(
        long,
        global = true,
        default_value = "/home/sancho20021/spw/localdeck/turntable.log"
    )]
    log_file: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the turntable application
    Run {
        /// Stereo pair assignments for each deck (e.g., "0" for 1 deck, "0,1" for 2 decks, "1,0" for 2 decks with swapped order, etc)
        #[arg(
            short('r'),
            long = "routing",
            value_delimiter = ',',
            default_value = "0"
        )]
        routing: Vec<usize>,

        /// Audio output device name or substring query
        #[arg(short('D'), long)]
        device: Option<String>,

        /// Audio callback buffer size
        #[arg(short, long, default_value_t = 256)]
        buffer: u32,

        /// Touchpad sensitivity factor
        #[arg(short('t'), long, default_value_t = 1.)]
        sensitivity: f64,

        /// Motor inertia parameter in seconds
        #[arg(short('i'), long, default_value_t = 0.5)]
        motor_inertia: f64,

        /// Nudge / Pitch bend responsiveness
        #[arg(short('n'), long, default_value_t = 1., allow_negative_numbers = true)]
        nudge: f32,
    },
}

fn main() {
    let args = Cli::parse();

    if let Some(parent) = args.log_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // 3. Open or create the target log file
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log_file)
        .unwrap_or_else(|err| panic!("Failed to open log file at {:?}: {}", args.log_file, err));

    // 4. Initialize env_logger with target pipe
    env_logger::builder()
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .init();

    info!("Initialized logging to {:?}", args.log_file);

    match &args.command {
        Commands::Run {
            routing,
            device,
            buffer,
            sensitivity,
            motor_inertia,
            nudge,
        } => {
            log::info!("Starting Turntable: {:?}", args.command);
            start(
                &routing,
                device.as_deref(),
                *motor_inertia,
                *sensitivity,
                *buffer,
                *nudge,
            );
        }
    }
}
