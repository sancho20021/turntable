mod decoder;
mod interpolation;
mod read_touchpad;
mod scratch;
mod stereo_frame;
mod touchpad_state;

use std::path::PathBuf;

use clap::Parser;

use crate::decoder::load_file;

#[derive(Parser, Debug)]
#[command(author, version, about = "Turntable Scratch Engine CLI", long_about = None)]
struct Args {
    /// Path to the audio track file to load (e.g., track.wav)
    #[arg(value_name = "AUDIO_TRACK")]
    input: PathBuf,
    /// Path to export telemetry CSV data on exit
    #[arg(short, long, value_name = "FILE")]
    mouse_data: Option<PathBuf>,

    /// Path to export deck (playhead) telemetry CSV data on exit
    #[arg(short, long, value_name = "FILE")]
    deck_data: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    println!("Loading: {}", args.input.to_string_lossy());
    let samples = load_file(&args.input.to_string_lossy()).unwrap();
    if samples.is_empty() {
        panic!("No audio decoded");
    }
    println!("Decoded {} frames", samples.len());
    scratch::run(samples, args.mouse_data, args.deck_data);
}
