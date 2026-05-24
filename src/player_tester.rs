use anyhow::bail;

use crate::{decoder::load_file, stereo_frame::StereoFrame};

/// CLI function to run a player on the audio file passed as the first argument
pub fn run_player(play: impl FnOnce(Vec<StereoFrame>) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        bail!("Usage: cargo run --release -- <audio-file>");
    }

    let path = &args[1];

    println!("Loading: {path}");

    let samples = load_file(path)?;

    if samples.is_empty() {
        bail!("No audio decoded");
    }
    println!("Decoded {} frames", samples.len());
    play(samples)?;
    Ok(())
}
