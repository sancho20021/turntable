use std::time::{Duration, Instant};

use cpal::{
    BufferSize, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rtrb::Producer;

use crate::{
    interpolation,
    scratch::record::InterpolatedRecord,
    scratchv2::{platter_audio_processor::PlatterAudioProcessor, virtual_platter::PlatterSample},
    stereo_frame::StereoFrame,
};

/// Main app loop
///
/// The updates are sent no more often then the specified frequency
pub fn start(
    speed: f64,
    samples: Vec<StereoFrame>,
    extra_lattency: Duration,
    platter_update_freq_hz: f64,
    jitter_fac: f64,
) {
    let (stream, mut platter) =
        start_deck(samples, extra_lattency, platter_update_freq_hz, jitter_fac).unwrap();

    let pause = Duration::from_secs_f64(1.0 / platter_update_freq_hz);
    let pos_step_nanos = (pause.as_nanos() as f64 * speed).floor() as u64;
    let mut platter_pos_nanos = 0;
    loop {
        let sample = PlatterSample {
            time: Instant::now(),
            record_pos: platter_pos_nanos,
        };
        println!("pushing {sample:?}");
        match platter.push(sample) {
            Ok(_) => (),
            Err(_) => {
                println!("Platter buffer IS FULL, terminating");
                return;
            }
        }
        platter_pos_nanos += pos_step_nanos;
        std::thread::sleep(pause);
    }
}

fn start_deck(
    samples: Vec<StereoFrame>,
    extra_lattency: Duration,
    platter_update_freq_hz: f64,
    jitter_fac: f64,
) -> anyhow::Result<(Stream, Producer<PlatterSample>)> {
    let host = cpal::default_host();

    let device = host.default_output_device().expect("No output device");
    let sample_rate = 44100;

    let buffer_size = 1024;

    let config = StreamConfig {
        channels: 2,
        sample_rate,
        // buffer_size: BufferSize::Fixed(4096),  // for testing purposes to make glitches easily hearable
        buffer_size: BufferSize::Fixed(buffer_size),
    };

    // let mut config = device.default_output_config()?;

    println!("Output config: {:?}", config);

    let record = InterpolatedRecord::new(samples, interpolation::Linear);
    let (mut processor, platter) = PlatterAudioProcessor::new(
        record,
        sample_rate as usize,
        buffer_size as usize,
        extra_lattency,
        platter_update_freq_hz,
        jitter_fac,
    );

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            processor.write_frames(data);
        },
        move |err| {
            eprintln!("audio error: {err}");
        },
        None,
    )?;

    stream.play()?;
    Ok((stream, platter))
}
