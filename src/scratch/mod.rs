use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{
    deck::interpolator,
    player_tester,
    scratch::{controller::ScratchController, mouse_processor::MouseProcessor, record::Record},
    stereo_frame::StereoFrame,
};

mod controller;
mod debugger;
mod mouse_analyzer;
mod mouse_processor;
mod record;
mod scratcher;

pub const SENSITIVITY: f64 = 100.;

pub fn play_audio(
    controller: Arc<ScratchController>,
    samples: Vec<StereoFrame>,
) -> anyhow::Result<()> {
    let host = cpal::default_host();

    let device = host.default_output_device().expect("No output device");

    let config = device.default_output_config()?;

    println!("Output config: {:?}", config);

    let channels = config.channels() as usize;
    assert_eq!(channels, 2);

    let record = Record::new(samples, interpolator::Linear);

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            scratcher::write_frames(data, &record, &controller);
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

fn run_fn(samples: Vec<StereoFrame>) -> anyhow::Result<()> {
    let controller = Arc::new(ScratchController::new(0., SENSITIVITY));
    let mouse_processor = MouseProcessor::new(Arc::clone(&controller), 0.5);
    let _ = controller::spawn_scratch_input(mouse_processor);
    debugger::monitor_controller(Arc::clone(&controller));
    play_audio(controller, samples)
}

pub fn run() {
    player_tester::run_player(run_fn).unwrap();
}
