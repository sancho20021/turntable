// use anyhow::{Result, bail};

// use std::fs::File;
// use std::time::Instant;

// use symphonia::core::audio::{AudioBufferRef, Signal};
// use symphonia::core::codecs::DecoderOptions;
// use symphonia::core::formats::FormatOptions;
// use symphonia::core::io::MediaSourceStream;
// use symphonia::core::meta::MetadataOptions;
// use symphonia::default::{get_codecs, get_probe};

// fn main() -> Result<()> {
//     let time_start = Instant::now();


//     let args: Vec<String> = std::env::args().collect();

//     if args.len() < 2 {
//         bail!("Usage: cargo run --release -- <audio-file>");
//     }

//     let path = &args[1];

//     println!("Loading: {path}");

//     let file = File::open(path)?;

//     let mss = MediaSourceStream::new(Box::new(file), Default::default());

//     let hint = symphonia::core::probe::Hint::new();

//     println!("Media stream created");

//     let probed = get_probe().format(
//         &hint,
//         mss,
//         &FormatOptions::default(),
//         &MetadataOptions::default(),
//     ).expect("Unsupported format");

//     println!("Get probe called");

//     let mut format = probed.format;

//     let track = format.default_track().expect("No default audio track");

//     println!("Codec: {:?}", track.codec_params.codec);
//     println!("Sample rate: {:?}", track.codec_params.sample_rate);
//     println!("Channels: {:?}", track.codec_params.channels);

//     let mut decoder = get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

//     let mut total_frames = 0usize;
//     let mut decoded_packets = 0usize;

//     loop {
//         let packet = match format.next_packet() {
//             Ok(packet) => packet,
//             Err(_) => break,
//         };

//         let decoded = decoder.decode(&packet)?;

//         decoded_packets += 1;

//         match decoded {
//             AudioBufferRef::F32(buf) => {
//                 total_frames += buf.frames();
//             }

//             AudioBufferRef::S16(buf) => {
//                 total_frames += buf.frames();
//             }

//             _ => {}
//         }
//     }

//     println!("Decoded packets: {decoded_packets}");
//     println!("Total frames: {total_frames}");

//     let elapsed = time_start.elapsed();
//     println!("Took {}ms", elapsed.as_millis());

//     Ok(())
// }
