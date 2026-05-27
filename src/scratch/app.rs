use std::{path::PathBuf, str::FromStr, sync::Arc, time::Instant};

use cpal::{
    BufferSize, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use sdl2::{event::Event, keyboard::Keycode, mouse::MouseButton};

use crate::{
    interpolation,
    scratch::{
        debugger,
        deck_event_handler::{DeckEvent, DeckEventHandler},
        deck_monitor::DeckMonitor,
        mouse_analyzer::MovementRecorder,
        record::InterpolatedRecord,
        scratcher::Scratcher,
        shared_state::ScratchState,
    },
    stereo_frame::StereoFrame,
};

pub struct Application {
    deck_event_handler: DeckEventHandler,
    scratch_state: Arc<ScratchState>,
    deck_monitor: DeckMonitor,
}

fn to_deck_event(event: Event) -> Option<DeckEvent> {
    match event {
        Event::MouseMotion { x, .. } => Some(DeckEvent::MouseMotion(x)),

        Event::MouseButtonDown {
            mouse_btn: MouseButton::Left,
            x,
            ..
        } => Some(DeckEvent::MouseDown(x)),

        Event::MouseButtonUp {
            mouse_btn: MouseButton::Left,
            x,
            ..
        } => Some(DeckEvent::MouseUp(x)),

        Event::KeyDown { keycode, .. } => {
            if let Some(key) = keycode {
                match key {
                    Keycode::R => Some(DeckEvent::KeyReset),
                    Keycode::Up => Some(DeckEvent::KeyUp),
                    Keycode::Down => Some(DeckEvent::KeyDown),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

impl Application {
    pub fn new(mouse_telemetry: Option<PathBuf>, deck_telemetry: Option<PathBuf>) -> Self {
        let telemetry = path_or_devnull(mouse_telemetry);
        let deck_tele = path_or_devnull(deck_telemetry);
        let recorder = MovementRecorder::new(&telemetry);
        let scratch_state = Arc::new(ScratchState::new(0.));
        let deck_event_handler = DeckEventHandler::new(Arc::clone(&scratch_state), recorder);
        let deck_monitor = DeckMonitor::new(Arc::clone(&scratch_state), &deck_tele);
        Self {
            deck_event_handler,
            scratch_state,
            deck_monitor,
        }
    }

    /// Main app loop
    pub fn start(&mut self, samples: Vec<StereoFrame>) {
        let sdl = sdl2::init().unwrap();
        let video = sdl.video().unwrap();

        let _window = video
            .window("scratch input", 600, 300)
            .position_centered()
            .build()
            .unwrap();

        let mut pump = sdl.event_pump().unwrap();
        let start = Instant::now();
        self.deck_monitor.start(start);
        self.deck_event_handler.set_recorder_start(start);
        debugger::monitor_scratch_state(Arc::clone(&self.scratch_state));

        let stream = start_deck(Arc::clone(&self.scratch_state), SENSITIVITY, samples).unwrap();

        // // fake mouse for testing:
        // println!("Pumping raw synthetic events into the scratcher... Press Ctrl+C to kill.");
        // let mut fake_x = 0;
        // let mut direction = 1;
        // const SPEED: i32 = 1; // How many pixels to move per step
        // const MAX_RANGE: i32 = 12000; // Peak "virtual" screen width

        // self.deck_event_handler
        //     .handle_event(DeckEvent::MouseDown(fake_x));

        // loop {
        //     for event in pump.poll_iter() {
        //         if let Event::Quit { .. } = event {
        //             println!("Stopping the app");
        //             self.deck_event_handler.stop_recorder();
        //             self.deck_monitor.finish();
        //             return;
        //         }
        //     }

        //     // Calculate next fake coordinate oscillating back and forth
        //     fake_x += direction * SPEED;
        //     if fake_x >= MAX_RANGE || fake_x <= 0 {
        //         direction *= -1;
        //     }

        //     // Spawn the event and feed it raw
        //     let fake_event = DeckEvent::MouseMotion(fake_x);
        //     self.deck_event_handler.handle_event(fake_event);

        //     // Sleep 2ms (~500Hz event generation rate)
        //     // Gives your audio engine a workout without spinning a CPU core to 100%
        //     std::thread::sleep(std::time::Duration::from_millis(8));
        // }

        for event in pump.wait_iter() {
            if let Event::Quit { .. } = event {
                println!("Stopping the app");
                self.deck_event_handler.stop_recorder();
                self.deck_monitor.finish();
                return;
            }
            if let Some(event) = to_deck_event(event) {
                self.deck_event_handler.handle_event(event);
            }
        }
    }
}

pub const SENSITIVITY: f64 = 300.;

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

pub fn start_deck(
    scratch_state: Arc<ScratchState>,
    sensitivity: f64,
    samples: Vec<StereoFrame>,
) -> anyhow::Result<Stream> {
    let host = cpal::default_host();

    let device = host.default_output_device().expect("No output device");

    let config = StreamConfig {
        channels: 2,
        sample_rate: 44100,
        // buffer_size: BufferSize::Fixed(4096),  // for testing purposes to make glitches easily hearable
        buffer_size: BufferSize::Fixed(512),

    };

    // let mut config = device.default_output_config()?;

    println!("Output config: {:?}", config);

    let record = InterpolatedRecord::new(samples, interpolation::Linear);
    let mut scratcher = Scratcher::new(
        record,
        scratch_state,
        sensitivity,
        config.sample_rate as f64,
    );

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
    Ok(stream)
}
