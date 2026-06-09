mod deck_event;
mod decoder;
mod interpolation;
mod read_touchpad;
mod scratch;
mod scratchv2;
mod sdl_deck_event;
mod stereo_frame;
mod touchpad_state;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::decoder::load_file;

#[derive(Parser, Debug)]
#[command(author, version, about = "Turntable Scratch Engine CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the original Turntable Scratch Engine application
    Original {
        /// Path to the audio track file to load (e.g., track.wav)
        #[arg(value_name = "AUDIO_TRACK")]
        input: PathBuf,

        /// Path to export telemetry CSV data on exit
        #[arg(short, long, value_name = "FILE")]
        mouse_data: Option<PathBuf>,

        /// Path to export deck (playhead) telemetry CSV data on exit
        #[arg(short, long, value_name = "FILE")]
        deck_data: Option<PathBuf>,
    },
    /// Run the new V2 testing turntable (no mouse tracking, synthetic events)
    V2 {
        /// Path to the audio track file to load (e.g., track.wav)
        #[arg(value_name = "AUDIO_TRACK")]
        input: PathBuf,

        /// Target frequency for platter updates in Hz
        #[arg(short, long, default_value_t = 100.0)]
        freq: f64,

        /// Playback speed multiplier
        #[arg(short, long, default_value_t = 1.0)]
        speed: f64,

        /// Audio callback buffer size
        #[arg(short, long, default_value_t = 512)]
        buffer: u32,

        /// Touchpad sensitivity factor
        #[arg(short('t'), long, default_value_t = 1.)]
        sensitivity: f64,
    },
}

fn main() {
    let args = Cli::parse();

    match args.command {
        Commands::Original {
            input,
            mouse_data,
            deck_data,
        } => {
            println!("Starting Original App...");
            println!("Loading: {}", input.to_string_lossy());

            let samples = load_file(&input.to_string_lossy()).unwrap();
            if samples.is_empty() {
                panic!("No audio decoded");
            }
            println!("Decoded {} frames", samples.len());

            let mut app = scratch::app::Application::new(mouse_data, deck_data);
            app.start(samples);
        }
        Commands::V2 {
            input,
            freq,
            speed,
            buffer,
            sensitivity,
        } => {
            println!(
                "Starting V2 App (platter update Freq: {:.2}Hz, Speed: {}x, buffer: {})...",
                freq, speed, buffer
            );
            println!("Loading: {}", input.to_string_lossy());

            let samples = load_file(&input.to_string_lossy()).unwrap();
            if samples.is_empty() {
                panic!("No audio decoded");
            }
            println!("Decoded {} frames", samples.len());

            scratchv2::app::start(speed, sensitivity, samples, freq, buffer);
        }
    }
}
