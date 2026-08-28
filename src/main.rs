mod app;
mod deck_controller;
mod deck_event;
mod deck_thread;
mod deck_worker;
mod decoder;
mod filters;
mod physical_speed;
mod platter_audio_processor;
mod platter_driver;
mod record;
mod record_changer;
mod samples_poller;
mod sdl_deck_event;
mod stereo_frame;
mod telemetry;
mod utils;
mod virtual_platter;

use clap::{Parser, Subcommand};
use log::info;

use crate::app::start;

#[derive(Parser, Debug)]
#[command(author, version, about = "Turntable Scratch Engine CLI", long_about = None)]
struct Cli {
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
    env_logger::builder()
        .target(env_logger::Target::Stdout)
        .init();
    info!("Initialized logging to stdout");

    let args = Cli::parse();

    match &args.command {
        Commands::Run {
            routing,
            device,
            buffer,
            sensitivity,
            motor_inertia,
            nudge,
        } => {
            println!("Starting Turntable: {:?}", args.command);
            println!("Drag and drop a music file to start");

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
