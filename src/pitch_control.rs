//
// Minimal vinyl-style realtime pitch player.
// Single file, clean module architecture.
//
// Usage:
//   cargo run --release -- song.mp3
//
// Controls:
//   u = pitch up
//   d = pitch down
//   r = reset speed
//   q = quit
//
// Cargo.toml:
//
//

use anyhow::{Result, bail};
use atomic_float::AtomicF64;
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{
    audio_engine::AudioEngine,
    deck::interpolator::{self, Interpolator}, decoder::load_file,
};

pub fn play_audio<Int: Interpolator>(
    mut engine: AudioEngine<Int>,
    speed: Arc<AtomicF64>,
) -> Result<()> {
    let host = cpal::default_host();

    let device = host.default_output_device().expect("No output device");

    let config = device.default_output_config()?;

    println!("Output config: {:?}", config);

    let channels = config.channels() as usize;

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            let current_speed = speed.load(std::sync::atomic::Ordering::Relaxed);

            engine.transport.set_speed(current_speed);

            for frame in data.chunks_mut(channels) {
                let s = engine.next_frame();

                frame[0] = s.l;

                if channels > 1 {
                    frame[1] = s.r;
                }
            }
        },
        move |err| {
            eprintln!("audio error: {err}");
        },
        None,
    )?;

    stream.play()?;

    loop {
        std::thread::park();
    }
}

fn play_with_pitch() -> Result<()> {
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

    let engine = AudioEngine::new(samples, interpolator::Linear);

    let speed = Arc::new(AtomicF64::new(1.0));

    // input thread
    {
        let speed = speed.clone();

        std::thread::spawn(move || {
            use std::io::{Read, stdin};

            println!();
            println!("Controls:");
            println!("  u = speed up");
            println!("  d = slow down");
            println!("  r = reset");
            println!("  q = quit");
            println!();

            loop {
                let mut buf = [0u8; 1];

                stdin().read_exact(&mut buf).unwrap();

                match buf[0] as char {
                    'u' => {
                        let s = speed.load(std::sync::atomic::Ordering::Relaxed) + 0.05;

                        speed.store(s, std::sync::atomic::Ordering::Relaxed);

                        println!("speed = {:.2}", s);
                    }

                    'd' => {
                        let s = speed.load(std::sync::atomic::Ordering::Relaxed) - 0.05;

                        speed.store(s, std::sync::atomic::Ordering::Relaxed);

                        println!("speed = {:.2}", s);
                    }

                    'r' => {
                        speed.store(1.0, std::sync::atomic::Ordering::Relaxed);

                        println!("speed = 1.00");
                    }

                    'q' => {
                        std::process::exit(0);
                    }

                    _ => {}
                }
            }
        });
    }

    play_audio(engine, speed)
}
