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
use rtrb::{Consumer, Producer};
use sdl2::event::Event;

use crate::{
    decoder::SAMPLE_RATE,
    record::Record,
    scratchv2::{
        deck_controller::DeckController,
        platter_audio_processor::PlatterAudioProcessor,
        record_changer::RecordChanger,
        samples_poller::SamplesPoller,
        virtual_platter::{ReadablePlatter, new_platter},
    },
    sdl_deck_event::to_deck_event,
    telemetry,
};

/// Main app loop
pub fn start(
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

    let (used_records_prod, used_records_cons) = rtrb::RingBuffer::new(3);
    let (new_record_prod, new_record_cons) = rtrb::RingBuffer::new(1);

    let (requested_record_snd, requested_rec_rcv) = bounded(100);

    let (write_platter, read_platter) = new_platter();

    let (mut controller, platter_driver, deck_worker) = DeckController::new(
        read_platter.clone(),
        write_platter,
        requested_record_snd,
        1.,
        touchpad_sensitivity,
        motor_inertia_secs,
        nudge_responsiveness,
    );
    let shutdown = Arc::new(AtomicBool::new(false));
    let send_external_events = deck_worker.get_event_sender();
    let controller_listener = deck_worker.listen_to_external_events(Arc::clone(&shutdown));

    let platter_update_freq_hz =
        PlatterAudioProcessor::platter_update_freq(buffer_frames_n as usize);
    log::info!("calculated platter update frequency is {platter_update_freq_hz}hz");

    let driver = platter_driver.start(platter_update_freq_hz, Arc::clone(&shutdown));

    let record_changer = RecordChanger::new(
        requested_rec_rcv,
        new_record_prod,
        used_records_cons,
        Arc::clone(&shutdown),
        send_external_events,
    )
    .start();

    let stream = start_deck(
        buffer_frames_n,
        used_records_prod,
        new_record_cons,
        read_platter,
    )
    .unwrap();

    for event in pump.wait_iter() {
        if let Event::Quit { .. } = event {
            break;
        }
        if let Some(event) = to_deck_event(event, Instant::now()) {
            let r = controller.handle_deck_event(event);
            if let Err(r) = r {
                log::error!("{r}");
            }
        }
    }

    println!("Stopping the app");
    drop(stream);
    shutdown.store(true, Ordering::Relaxed);
    let platter_driver = driver.join().map_err(|e| {
        log::error!("Platter driver panicked");
        e
    });
    if let Err(_) = record_changer.join() {
        log::error!("Record changer panicked");
    }
    if let Err(_) = controller_listener.join() {
        log::error!("Controller listener panicked");
    }

    let mut tracers = vec![controller.tracer];
    if let Ok(driver) = platter_driver {
        tracers.push(driver.tracer);
    }

    telemetry::save_traces_to_file(tracers, "trace.csv").expect("Failed to save telemetry");
}

pub fn list_output_devices() -> anyhow::Result<Vec<Device>> {
    let host = cpal::default_host();

    // let devices = host
    //     .output_devices()?
    //     .filter(|device| {
    //         device
    //             .id()
    //             .ok()
    //             .map(|id| id.to_string())
    //             .and_then(|id| id.strip_prefix("alsa:").map(str::to_string))
    //             .is_some_and(|rest| rest == "default" || rest.starts_with("dmix:CARD="))
    //     })
    //     .unique_by(|device| device.id().unwrap().to_string())
    //     .collect();

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
fn start_deck(
    // buffer size in frames
    buffer_frames_n: u32,
    used_records: Producer<Record>,
    new_record: Consumer<Record>,
    platter: ReadablePlatter,
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

    let chosen_config = StreamConfig {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        buffer_size: BufferSize::Fixed(buffer_frames_n),
    };

    log::info!("Supported device output configs:");
    let supported_configs: Vec<_> = device.supported_output_configs()?.collect();
    for config in &supported_configs {
        log::info!("- {config:?}");
    }
    ensure_config_supported(&supported_configs, &chosen_config)?;
    println!("Stream config: {:?}", chosen_config);

    let processor = PlatterAudioProcessor::new(platter, new_record, used_records);
    let mut samples_poller = SamplesPoller::new(buffer_frames_n as usize, [processor]);

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
