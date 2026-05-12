use anyhow::Result;

use std::fs::File;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::default::{get_codecs, get_probe};

use crate::stereo_frame::StereoFrame;

/// Loads and decodes the whole music file into RAM
pub fn load_file(path: &str) -> Result<Vec<StereoFrame>> {
    let file = File::open(path)?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let hint = symphonia::core::probe::Hint::new();

    let probed = get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;

    let mut format = probed.format;

    let track = format.default_track().expect("No default audio track");

    let mut decoder = get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut samples = Vec::<StereoFrame>::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(_) => break,
        };

        let decoded = decoder.decode(&packet)?;

        match decoded {
            AudioBufferRef::F32(buf) => {
                let channels = buf.spec().channels.count();

                for i in 0..buf.frames() {
                    let l = buf.chan(0)[i];

                    let r = if channels > 1 { buf.chan(1)[i] } else { l };

                    samples.push(StereoFrame { l, r });
                }
            }

            AudioBufferRef::S16(buf) => {
                let channels = buf.spec().channels.count();

                for i in 0..buf.frames() {
                    let l = buf.chan(0)[i] as f32 / i16::MAX as f32;

                    let r = if channels > 1 {
                        buf.chan(1)[i] as f32 / i16::MAX as f32
                    } else {
                        l
                    };

                    samples.push(StereoFrame { l, r });
                }
            }

            _ => {}
        }
    }

    Ok(samples)
}
