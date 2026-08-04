mod deck_event;
mod decoder;
mod record;
mod scratchv2;
mod sdl_deck_event;
mod stereo_frame;
mod utils;
mod telemetry;

use clap::{Parser, Subcommand};
use log::info;

#[derive(Parser, Debug)]
#[command(author, version, about = "Turntable Scratch Engine CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the new V2 testing turntable (no mouse tracking, synthetic events)
    V2 {
        /// Audio callback buffer size
        #[arg(short, long, default_value_t = 128)]
        buffer: u32,

        /// Touchpad sensitivity factor
        #[arg(short('t'), long, default_value_t = 1.)]
        sensitivity: f64,

        /// Motor inertia parameter in seconds
        #[arg(short('i'), long, default_value_t = 0.5)]
        motor_inertia: f64,
    },
}

fn main() {
    env_logger::builder()
        .target(env_logger::Target::Stdout)
        .init();
    info!("Initialized logging to stdout");

    let args = Cli::parse();

    match args.command {
        Commands::V2 {
            buffer,
            sensitivity,
            motor_inertia,
        } => {
            let sample_rate = 44100;
            println!(
                "Starting V2 App (buffer: {}, touchpad sensitivity: {sensitivity:.2}, motor inertia: {motor_inertia:.2}).",
                buffer
            );
            println!("Drag and drop a music file to start");

            scratchv2::app::start(motor_inertia, sensitivity, buffer, sample_rate);
        }
    }
}
