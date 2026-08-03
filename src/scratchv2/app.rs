use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use cpal::{
    BufferSize, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam::channel::bounded;
use rtrb::{Consumer, Producer};
use sdl2::event::Event;

use crate::{
    record::Record,
    scratchv2::{
        deck_controller::DeckController,
        platter_audio_processor::PlatterAudioProcessor,
        record_changer::RecordChanger,
        virtual_platter::{ReadablePlatter, WritablePlatter, new_platter},
    },
    sdl_deck_event::to_deck_event,
};

/// Main app loop
pub fn start(
    motor_inertia_secs: f64,
    touchpad_sensitivity: f64,
    // buffer in frames
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

    let (used_records_prod, used_records_cons) = rtrb::RingBuffer::new(3);
    let (new_record_prod, new_record_cons) = rtrb::RingBuffer::new(1);

    let (requested_record_snd, requested_rec_rcv) = bounded(100);

    let (write_platter, read_platter) = new_platter();

    let (controller, platter_driver) = DeckController::new(
        read_platter.clone(),
        write_platter,
        requested_record_snd,
        1.,
        touchpad_sensitivity,
        motor_inertia_secs,
    );
    let controller = Arc::new(controller);
    let shutdown = Arc::new(AtomicBool::new(false));
    let controller_listener =
        Arc::clone(&controller).listen_to_external_events(Arc::clone(&shutdown));

    let platter_update_freq_hz =
        PlatterAudioProcessor::platter_update_freq(sample_rate as usize, buffer_size as usize);
    log::info!("calculated platter update frequency is {platter_update_freq_hz}hz");

    let driver = platter_driver.start(platter_update_freq_hz, Arc::clone(&shutdown));

    let record_changer = RecordChanger::new(
        requested_rec_rcv,
        new_record_prod,
        used_records_cons,
        Arc::clone(&shutdown),
        controller.get_event_sender(),
    )
    .start();

    let stream = start_deck(
        buffer_size,
        sample_rate,
        used_records_prod,
        new_record_cons,
        read_platter,
    )
    .unwrap();

    for event in pump.wait_iter() {
        if let Event::Quit { .. } = event {
            println!("Stopping the app");
            drop(stream);
            shutdown.store(true, Ordering::Relaxed);
            if let Err(_) = driver.join() {
                log::error!("Platter driver panicked");
            }
            if let Err(_) = record_changer.join() {
                log::error!("Record changer panicked");
            }
            if let Err(_) = controller_listener.join() {
                log::error!("Controller listener panicked");
            }
            return;
        }
        if let Some(event) = to_deck_event(event) {
            let r = controller.handle_deck_event(event);
            if let Err(r) = r {
                log::error!("{r}");
            }
        }
    }
}

/// Start audio thread
fn start_deck(
    buffer_size: u32,
    sample_rate: u32,
    used_records: Producer<Record>,
    new_record: Consumer<Record>,
    platter: ReadablePlatter,
) -> anyhow::Result<Stream> {
    let host = cpal::default_host();

    let device = host.default_output_device().expect("No output device");

    let config = StreamConfig {
        channels: 2,
        sample_rate,
        buffer_size: BufferSize::Fixed(buffer_size),
    };

    println!("Stream config: {:?}", config);

    let mut processor =
        PlatterAudioProcessor::new(sample_rate as usize, platter, new_record, used_records);

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
    Ok(stream)
}
