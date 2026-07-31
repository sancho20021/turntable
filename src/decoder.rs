use anyhow::{Result, anyhow, bail};
use symphonium::DecodeConfig;

use std::{num::NonZeroU32, path::Path};

use crate::stereo_frame::StereoFrame;

/// Loads and decodes the whole music file into RAM
pub fn load_file(sample_rate: u32, path: &Path) -> Result<Vec<StereoFrame>> {
    // Probe the audio file.
    let probed = symphonium::probe_from_file(
        path,
        // A custom codec prober. Set to `None` to use the default one from symphonia.
        None,
    )?;
    let audio_data_f32 = symphonium::decode_f32(
        probed,
        &DecodeConfig::default(),
        Some(NonZeroU32::new(sample_rate).ok_or(anyhow!("sample rate must be non-zero"))?),
        None,
        None,
    )?;

    if audio_data_f32.channels() != 2 {
        bail!(
            "Audio file must have 2 channels but it has {}",
            audio_data_f32.channels()
        );
    }
    let mut data = audio_data_f32.data.into_iter();
    let left = data.next().unwrap();
    let right = data.next().unwrap();

    let samples: Vec<_> = left
        .into_iter()
        .zip(right)
        .map(|(l, r)| StereoFrame { l, r })
        .collect();

    if samples.is_empty() {
        bail!("empty audio decoded");
    }

    log::info!("Decoded {} frames", samples.len());
    Ok(samples)
}
