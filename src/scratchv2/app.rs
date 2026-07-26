use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use cpal::{
    BufferSize, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use sdl2::event::Event;

use crate::{
    interpolation,
    record::InterpolatedRecord,
    scratchv2::{
        deck_controller::ScratchController,
        platter_audio_processor::PlatterAudioProcessor,
        platter_driver,
        virtual_platter::{ReadablePlatter, WritablePlatter, new_platter},
    },
    sdl_deck_event::to_deck_event,
    stereo_frame::StereoFrame,
};

/// Main app loop
pub fn start(
    touchpad_sensitivity: f64,
    samples: Vec<StereoFrame>,
    platter_update_freq_hz: f64,
    buffer_size: u32,
    sample_rate: u32,
) {
    let sdl = sdl2::init().unwrap();
    let video = sdl.video().unwrap();

    let _window = video
        .window("scratch input", 600, 300)
        .position_centered()
        .build()
        .unwrap();

    let mut pump = sdl.event_pump().unwrap();
    let (stream, write_platter, read_platter) =
        start_deck(samples, buffer_size, sample_rate).unwrap();
    let (mut controller, platter_src) =
        ScratchController::new(read_platter, write_platter, 1., touchpad_sensitivity);
    let platter_shutdown = Arc::new(AtomicBool::new(false));
    let driver = platter_driver::spawn_platter_driver(
        platter_src,
        platter_update_freq_hz,
        Arc::clone(&platter_shutdown),
    );

    for event in pump.wait_iter() {
        if let Event::Quit { .. } = event {
            println!("Stopping the app");
            drop(stream);
            platter_shutdown.store(true, Ordering::Relaxed);
            if let Err(_) = driver.join() {
                eprintln!("Platter driver panicked");
            }
            return;
        }
        if let Some(event) = to_deck_event(event) {
            controller.handle_deck_event(event);
        }
    }
}

fn start_deck(
    samples: Vec<StereoFrame>,
    buffer_size: u32,
    sample_rate: u32,
) -> anyhow::Result<(Stream, WritablePlatter, ReadablePlatter)> {
    let host = cpal::default_host();

    let device = host.default_output_device().expect("No output device");

    let config = StreamConfig {
        channels: 2,
        sample_rate,
        buffer_size: BufferSize::Fixed(buffer_size),
    };

    println!("Output config: {:?}", config);

    let record = InterpolatedRecord::new(samples, interpolation::Linear);
    let (write_platter, read_platter) = new_platter();
    let mut processor =
        PlatterAudioProcessor::new(record, sample_rate as usize, read_platter.clone());

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
    Ok((stream, write_platter, read_platter))
}
