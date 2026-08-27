use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use cpal::{
    BufferSize, Device, SampleFormat, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfigRange,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam::channel::bounded;
use itertools::Itertools;
use sdl2::event::Event;

use crate::{
    deck_controller::{self},
    deck_thread::DeckJoinHandle,
    decoder::SAMPLE_RATE,
    platter_audio_processor::{AudioProcessorHandles, PlatterAudioProcessor},
    record_changer,
    samples_poller::{DeckRouting, SamplesPoller},
    sdl_deck_event::DeckEventMapper,
    telemetry,
    utils::unzip_array3,
};

/// Main app loop
pub fn start(
    motor_inertia_secs: f64,
    touchpad_sensitivity: f64,
    // buffer in frames
    buffer_frames_n: u32,
    nudge_responsiveness: f32,
) {
    run_app::<1, 2>(
        [0],
        motor_inertia_secs,
        touchpad_sensitivity,
        buffer_frames_n,
        nudge_responsiveness,
    );
}

fn run_app<const DECKS: usize, const CHANNELS: usize>(
    deck_routing: [usize; DECKS],
    motor_inertia_secs: f64,
    touchpad_sensitivity: f64,
    // buffer in frames
    buffer_frames_n: u32,
    nudge_responsiveness: f32,
) {
    let sdl = sdl2::init().unwrap();
    let video = sdl.video().unwrap();

    let _window = video
        .window("scratch input", 600, 300)
        .position_centered()
        .build()
        .unwrap();

    let mut pump = sdl.event_pump().unwrap();

    let (requested_record_snd, requested_rec_rcv) = bounded(3);
    let shutdown = Arc::new(AtomicBool::new(false));

    let deck_tuples = std::array::from_fn(|deck_idx| {
        deck_controller::new_deck(
            deck_idx,
            1.0,
            touchpad_sensitivity,
            motor_inertia_secs,
            nudge_responsiveness,
            requested_record_snd.clone(),
            Arc::clone(&shutdown),
            buffer_frames_n as usize,
        )
    });

    let (mut controllers, deck_threads, audio_processor_handles) = unzip_array3(deck_tuples);

    let worker_channels: [_; DECKS] =
        std::array::from_fn(|i| deck_threads[i].deck_worker_channel());
    let started_deck_threads = deck_threads.map(|thread| thread.start());

    let routing = DeckRouting::<DECKS, CHANNELS>::try_new(deck_routing).unwrap();
    let stream = start_deck(buffer_frames_n, audio_processor_handles, routing).unwrap();

    let record_changer =
        record_changer::start(requested_rec_rcv, worker_channels, Arc::clone(&shutdown));

    let mut event_mapper = DeckEventMapper::<DECKS>::new();

    for event in pump.wait_iter() {
        if let Event::Quit { .. } = event {
            break;
        }

        if let Some((deck_idx, event)) = event_mapper.to_deck_event(event, Instant::now()) {
            if let Some(controller) = controllers.get_mut(deck_idx) {
                if let Err(r) = controller.handle_deck_event(event) {
                    log::error!("[Deck {deck_idx}] {r}");
                }
            } else {
                log::warn!("Received event for non-existent deck index: {deck_idx}");
            }
        }
    }

    // 4. Teardown & Thread Joining
    println!("Stopping the app");
    drop(stream);
    shutdown.store(true, Ordering::Relaxed);

    if let Err(_) = record_changer.join() {
        log::error!("Record changer panicked");
    }

    // Join all platter driver threads
    let platter_drivers = started_deck_threads.map(DeckJoinHandle::join);

    // 5. Consolidate telemetry tracers across controllers and driver threads
    let mut tracers = Vec::with_capacity(DECKS * 2);
    for controller in controllers {
        tracers.push(controller.tracer);
    }
    for driver in platter_drivers.into_iter().flatten() {
        tracers.push(driver.tracer);
    }

    telemetry::save_traces_to_file(tracers, "trace.csv").expect("Failed to save telemetry");
}

pub fn list_output_devices() -> anyhow::Result<Vec<Device>> {
    let host = cpal::default_host();

    let devices = host
        .output_devices()?
        .unique_by(|device| device.id().unwrap().to_string())
        .collect();

    Ok(devices)
}

fn ensure_config_supported(
    supported: &[SupportedStreamConfigRange],
    chosen: &StreamConfig,
) -> anyhow::Result<()> {
    let ok = supported.iter().any(|range| {
        range.channels() == chosen.channels
            && range.sample_format() == SampleFormat::F32
            && range.min_sample_rate() <= chosen.sample_rate
            && chosen.sample_rate <= range.max_sample_rate()
            && match (&chosen.buffer_size, range.buffer_size()) {
                (BufferSize::Fixed(n), SupportedBufferSize::Range { min, max }) => {
                    n >= min && n <= max
                }
                (BufferSize::Default, _) => true,
                _ => false,
            }
    });

    if ok {
        Ok(())
    } else {
        anyhow::bail!("device does not support requested config: {chosen:?} (format: F32)")
    }
}

/// Start audio thread
fn start_deck<const DECKS: usize, const CHANNELS: usize>(
    // buffer size in frames
    buffer_frames_n: u32,
    audio_processor_handles: [AudioProcessorHandles; DECKS],
    deck_routing: DeckRouting<DECKS, CHANNELS>,
) -> anyhow::Result<Stream> {
    let host = cpal::default_host();

    log::info!("Available output devices:");
    for device in list_output_devices()? {
        let id = device.id()?;
        let device = device.description()?;
        log::info!("id:{id}, desc:{device}");
    }

    let device = host.default_output_device().expect("No output device");
    log::info!(
        "Chosen device: id:{}, desc:{}",
        device.id()?,
        device.description()?
    );

    let target_channels = u16::try_from(CHANNELS)
        .map_err(|_| anyhow::anyhow!("Requested channels ({CHANNELS}) exceeds u16::MAX"))?;

    let chosen_config = StreamConfig {
        channels: target_channels,
        sample_rate: SAMPLE_RATE,
        buffer_size: BufferSize::Fixed(buffer_frames_n),
    };

    log::info!("Supported device output configs:");
    let supported_configs: Vec<_> = device.supported_output_configs()?.collect();
    for config in &supported_configs {
        log::info!("- {config:?}");
    }
    ensure_config_supported(&supported_configs, &chosen_config)?;
    log::info!("Stream config: {:?}", chosen_config);

    let processors = audio_processor_handles.map(PlatterAudioProcessor::new);
    let mut samples_poller = SamplesPoller::new(buffer_frames_n as usize, processors, deck_routing);

    let stream = device.build_output_stream(
        chosen_config,
        move |data: &mut [f32], _| {
            samples_poller.write_frames(data);
        },
        move |err| {
            eprintln!("audio error: {err}");
        },
        None,
    )?;

    stream.play()?;
    Ok(stream)
}
