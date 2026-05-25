use std::{
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{
    interpolation,
    scratch::{
        mouse_analyzer::MovementRecorder, mouse_processor::MouseProcessor,
        record::InterpolatedRecord, scratcher::Scratcher, shared_state::ScratchState,
    },
    stereo_frame::StereoFrame,
};

mod debugger;
mod deck_monitor;
mod mouse_analyzer;
mod mouse_processor;
mod record;
mod scratcher;
mod shared_state;

pub const SENSITIVITY: f64 = 300.;
pub const INERTIA_OMEGA: f64 = 40.;

pub fn play_audio(
    scratch_state: Arc<ScratchState>,
    sensitivity: f64,
    inertia_omega: f64,
    samples: Vec<StereoFrame>,
) -> anyhow::Result<()> {
    let host = cpal::default_host();

    let device = host.default_output_device().expect("No output device");

    let config = device.default_output_config()?;

    println!("Output config: {:?}", config);

    let channels = config.channels() as usize;
    assert_eq!(channels, 2);

    let record = InterpolatedRecord::new(samples, interpolation::Linear);
    let mut scratcher: Scratcher<InterpolatedRecord<interpolation::Linear>> =
        Scratcher::new(record, scratch_state, sensitivity, inertia_omega);

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            scratcher.write_frames(data);
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

fn path_or_devnull(path: Option<PathBuf>) -> PathBuf {
    path.unwrap_or({
        let null_device = if cfg!(target_os = "windows") {
            "NUL"
        } else {
            "/dev/null"
        };
        PathBuf::from_str(null_device).unwrap()
    })
}

pub fn run(samples: Vec<StereoFrame>, telemetry: Option<PathBuf>, deck: Option<PathBuf>) {
    let telemetry = path_or_devnull(telemetry);
    let deck_tele = path_or_devnull(deck);
    let deck_tele_shutdown = Arc::new(AtomicBool::new(false));

    let state = Arc::new(ScratchState::new(0.));
    deck_monitor::monitor_deck_playhead(
        Arc::clone(&state),
        Arc::clone(&deck_tele_shutdown),
        deck_tele,
    );
    let recorder = MovementRecorder::new(&telemetry);
    let mouse_processor = MouseProcessor::new(Arc::clone(&state), recorder, deck_tele_shutdown);
    let _ = shared_state::spawn_scratch_input(mouse_processor);
    debugger::monitor_scratch_state(Arc::clone(&state));
    play_audio(state, SENSITIVITY, INERTIA_OMEGA, samples).unwrap();
}
