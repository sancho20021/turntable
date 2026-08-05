mod deck_event;
mod decoder;
mod filters;
mod record;
mod scratchv2;
mod sdl_deck_event;
mod stereo_frame;
mod telemetry;
mod utils;

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
            nudge,
        } => {
            let sample_rate = 44100;
            println!("Starting Turntable: {:?}", args.command);
            println!("Drag and drop a music file to start");

            scratchv2::app::start(motor_inertia, sensitivity, buffer, sample_rate, nudge);
        }
    }
}
