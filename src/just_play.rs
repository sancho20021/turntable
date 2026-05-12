// //
// // Super minimal audio player.
// // - decode entire file into memory
// // - play sequentially
// // - no pitch control
// // - no transport abstraction
// // - no interpolation
// // - no threading
// //
// // Usage:
// //   cargo run --release -- song.mp3
// //
// // Cargo.toml:
// //
// // [dependencies]
// // anyhow = "1"
// // cpal = "0.15"
// // symphonia = { version = "0.5", features = ["mp3", "wav", "flac"] }
// //

// use anyhow::{bail, Result};

// mod types {
//     #[derive(Clone, Copy, Debug, Default)]
//     pub struct StereoFrame {
//         pub l: f32,
//         pub r: f32,
//     }
// }

// mod audio {
//     pub mod decoder {
//         use anyhow::Result;

//         use std::fs::File;

//         use symphonia::core::audio::{AudioBufferRef, Signal};
//         use symphonia::core::codecs::DecoderOptions;
//         use symphonia::core::formats::FormatOptions;
//         use symphonia::core::io::MediaSourceStream;
//         use symphonia::core::meta::MetadataOptions;
//         use symphonia::default::{get_codecs, get_probe};

//         use crate::types::StereoFrame;

//         pub fn load_file(path: &str) -> Result<Vec<StereoFrame>> {
//             let file = File::open(path)?;

//             let mss =
//                 MediaSourceStream::new(Box::new(file), Default::default());

//             let hint = symphonia::core::probe::Hint::new();

//             let probed = get_probe().format(
//                 &hint,
//                 mss,
//                 &FormatOptions::default(),
//                 &MetadataOptions::default(),
//             )?;

//             let mut format = probed.format;

//             let track = format
//                 .default_track()
//                 .expect("No default audio track");

//             let mut decoder = get_codecs().make(
//                 &track.codec_params,
//                 &DecoderOptions::default(),
//             )?;

//             let mut samples = Vec::<StereoFrame>::new();

//             loop {
//                 let packet = match format.next_packet() {
//                     Ok(packet) => packet,
//                     Err(_) => break,
//                 };

//                 let decoded = decoder.decode(&packet)?;

//                 match decoded {
//                     AudioBufferRef::F32(buf) => {
//                         let channels =
//                             buf.spec().channels.count();

//                         for i in 0..buf.frames() {
//                             let l = buf.chan(0)[i];

//                             let r = if channels > 1 {
//                                 buf.chan(1)[i]
//                             } else {
//                                 l
//                             };

//                             samples.push(
//                                 StereoFrame { l, r }
//                             );
//                         }
//                     }

//                     AudioBufferRef::S16(buf) => {
//                         let channels =
//                             buf.spec().channels.count();

//                         for i in 0..buf.frames() {
//                             let l = buf.chan(0)[i] as f32
//                                 / i16::MAX as f32;

//                             let r = if channels > 1 {
//                                 buf.chan(1)[i] as f32
//                                     / i16::MAX as f32
//                             } else {
//                                 l
//                             };

//                             samples.push(
//                                 StereoFrame { l, r }
//                             );
//                         }
//                     }

//                     _ => {}
//                 }
//             }

//             Ok(samples)
//         }
//     }

//     pub mod output {
//         use anyhow::Result;

//         use cpal::traits::{
//             DeviceTrait,
//             HostTrait,
//             StreamTrait,
//         };

//         use crate::types::StereoFrame;

//         pub fn play(
//             samples: Vec<StereoFrame>,
//         ) -> Result<()> {
//             let host = cpal::default_host();

//             let device = host
//                 .default_output_device()
//                 .expect("No output device");

//             let config =
//                 device.default_output_config()?;

//             println!("Output config: {:?}", config);

//             let channels =
//                 config.channels() as usize;

//             // playback cursor
//             let mut position = 0usize;

//             let stream = device.build_output_stream(
//                 &config.into(),
//                 move |data: &mut [f32], _| {
//                     for frame in
//                         data.chunks_mut(channels)
//                     {
//                         if position >= samples.len() {
//                             // silence after end
//                             frame[0] = 0.0;

//                             if channels > 1 {
//                                 frame[1] = 0.0;
//                             }

//                             continue;
//                         }

//                         let sample = samples[position];

//                         frame[0] = sample.l;

//                         if channels > 1 {
//                             frame[1] = sample.r;
//                         }

//                         position += 1;
//                     }
//                 },
//                 move |err| {
//                     eprintln!("audio error: {err}");
//                 },
//                 None,
//             )?;

//             stream.play()?;

//             loop {
//                 std::thread::park();
//             }
//         }
//     }
// }

// use audio::decoder::load_file;
// use audio::output::play;

// fn main() -> Result<()> {
//     let args: Vec<String> = std::env::args().collect();

//     if args.len() < 2 {
//         bail!("Usage: cargo run --release -- <audio-file>");
//     }

//     let path = &args[1];

//     println!("Loading: {path}");

//     let samples = load_file(path)?;

//     if samples.is_empty() {
//         bail!("No audio decoded");
//     }

//     println!("Decoded {} frames", samples.len());

//     play(samples)
// }
