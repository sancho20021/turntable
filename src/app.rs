use std::{
    array,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, bail};
use cpal::{
    BufferSize, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam::channel::bounded;
use sdl2::event::Event;

use turntable_lib::{
    deck_controller::{self, AppStatus},
    deck_thread::DeckJoinHandle,
    decoder::SAMPLE_RATE,
    input_profile::InputProfile,
    platter_audio_processor::{AudioProcessorHandles, PlatterAudioProcessor},
    ratatui::spawn_tui_thread,
    record_changer,
    samples_poller::{DeckRouting, SamplesPoller},
    sdl_deck_event::DeckEventMapper,
    telemetry,
    utils::unzip_array3,
};

/// Main app loop
macro_rules! dispatch_app {
    ($decks:expr, $routing_slice:expr, $device_query:expr, $($args:expr),* $(,)?) => {{
        let routing_array: [usize; $decks] = $routing_slice
            .try_into()
            .expect("Routing slice length does not match deck count");
        run_app::<$decks>(
            routing_array,
            $device_query,
            $($args),*
        );
    }};
}

/// Main app loop entrypoint.
pub fn start(
    deck_routing: &[usize],
    device_query: Option<&str>,
    motor_inertia_secs: f64,
    touchpad_sensitivity: f64,
    // buffer in frames
    buffer_frames_n: u32,
    nudge_responsiveness: f32,
) {
    let decks = deck_routing.len();

    match decks {
        1 => dispatch_app!(
            1,
            deck_routing,
            device_query,
            motor_inertia_secs,
            touchpad_sensitivity,
            buffer_frames_n,
            nudge_responsiveness
        ),
        2 => dispatch_app!(
            2,
            deck_routing,
            device_query,
            motor_inertia_secs,
            touchpad_sensitivity,
            buffer_frames_n,
            nudge_responsiveness
        ),
        3 => dispatch_app!(
            3,
            deck_routing,
            device_query,
            motor_inertia_secs,
            touchpad_sensitivity,
            buffer_frames_n,
            nudge_responsiveness
        ),
        4 => dispatch_app!(
            4,
            deck_routing,
            device_query,
            motor_inertia_secs,
            touchpad_sensitivity,
            buffer_frames_n,
            nudge_responsiveness
        ),
        _ => panic!("Maximum 4 decks supported"),
    }
}

fn run_app<const DECKS: usize>(
    deck_routing: [usize; DECKS],
    device_query: Option<&str>,
    motor_inertia_secs: f64,
    touchpad_sensitivity: f64,
    // buffer in frames
    buffer_frames_n: u32,
    nudge_responsiveness: f32,
) {
    let app_status = AppStatus::new();
    app_status.set(format!("Drag and drop a music file to start"));

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

    let input_profile = InputProfile::touchpad(touchpad_sensitivity);

    let deck_tuples = std::array::from_fn(|deck_idx| {
        deck_controller::new_deck(
            deck_idx,
            1.0,
            input_profile,
            motor_inertia_secs,
            nudge_responsiveness,
            requested_record_snd.clone(),
            Arc::clone(&shutdown),
            buffer_frames_n as usize,
            &app_status,
        )
    });

    let (mut controllers, deck_threads, audio_processor_handles) = unzip_array3(deck_tuples);

    let worker_channels: [_; DECKS] =
        std::array::from_fn(|i| deck_threads[i].deck_worker_channel());
    let started_deck_threads = deck_threads.map(|thread| thread.start());

    let stream = start_deck(
        buffer_frames_n,
        audio_processor_handles,
        deck_routing,
        device_query,
    )
    .unwrap();

    let record_changer = record_changer::start(
        requested_rec_rcv,
        worker_channels,
        app_status.clone(),
        Arc::clone(&shutdown),
    );

    let mut event_mapper = DeckEventMapper::<DECKS>::new(app_status.clone());

    // Spawn polling TUI thread
    let tui_handle = spawn_tui_thread::<DECKS>(
        Arc::clone(&event_mapper.active_deck),
        array::from_fn(|i| controllers[i].get_state()),
        array::from_fn(|i| controllers[i].get_platter()),
        app_status.clone(),
        Arc::clone(&shutdown),
    );

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
    log::info!("Stopping the app");
    drop(stream);
    shutdown.store(true, Ordering::Relaxed);

    if let Err(_) = record_changer.join() {
        log::error!("Record changer panicked");
    }

    // Join all platter driver threads
    let platter_drivers = started_deck_threads.map(DeckJoinHandle::join);

    if let Err(_) = tui_handle.join() {
        log::error!("TUI thread panicked");
    }

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

fn direction_score(device: &cpal::Device) -> u8 {
    match device.description().map(|d| d.direction()).ok() {
        Some(cpal::DeviceDirection::Output) => 2, // Highest priority
        Some(cpal::DeviceDirection::Duplex) => 1, // Fallback
        _ => 0,                                   // Input / Unknown
    }
}

fn max_channels(device: &cpal::Device) -> u16 {
    device
        .supported_output_configs()
        .map(|configs| configs.map(|c| c.channels()).max().unwrap_or(0))
        .unwrap_or(0)
}

/// Ranking key: (direction_score, max_channels)
/// Higher values win: e.g. Output (2) beats Duplex (1), then breaks ties by channel count.
fn device_rank(device: &cpal::Device) -> (u8, u16) {
    (direction_score(device), max_channels(device))
}

fn resolve_device(host: &cpal::Host, device_query: Option<&str>) -> anyhow::Result<cpal::Device> {
    let query = match device_query.map(str::trim).filter(|q| !q.is_empty()) {
        Some(q) => q.to_lowercase(),
        None => {
            log::info!("No device query specified; using default output device.");
            return host
                .default_output_device()
                .context("No default output device found on system");
        }
    };

    let all_devices: Vec<_> = host
        .output_devices()
        .context("Failed to query output devices")?
        .collect();

    let matches: Vec<_> = all_devices
        .into_iter()
        .filter(|dev| dev.to_string().to_lowercase().contains(&query))
        .collect();

    if matches.is_empty() {
        log::error!("Available output devices:");
        for dev in host.output_devices()? {
            log::error!("  - \"{dev}\"");
        }
        bail!("No audio output device found matching query: \"{query}\"");
    }

    if matches.len() == 1 {
        let device = matches.into_iter().next().unwrap();
        log::info!("Selected unique output device: \"{device}\"");
        return Ok(device);
    }

    let first_name = matches[0].to_string();
    let all_same_name = matches.iter().all(|dev| dev.to_string() == first_name);

    if !all_same_name {
        let matched_list = matches
            .iter()
            .map(|dev| format!("  - \"{dev}\""))
            .collect::<Vec<_>>()
            .join("\n");

        bail!(
            "Ambiguous audio device query \"{query}\": matches {} distinct devices:\n{matched_list}",
            matches.len()
        );
    }

    for device in &matches {
        log::info!("supported config of {device}:");
        for config in device.supported_output_configs()? {
            log::info!("- {config:?}");
        }
    }
    let best_match = matches.into_iter().max_by_key(device_rank).unwrap();

    log::warn!(
        "Multiple endpoints found sharing identical name \"{first_name}\". Disambiguated best endpoint (Output > Duplex, Max Channels)."
    );

    Ok(best_match)
}

/// Start audio thread
fn start_deck<const DECKS: usize>(
    // buffer size in frames
    buffer_frames_n: u32,
    audio_processor_handles: [AudioProcessorHandles; DECKS],
    deck_routing: [usize; DECKS],
    device_query: Option<&str>,
) -> anyhow::Result<Stream> {
    let host = cpal::default_host();
    log::info!("CPAL Host API: {:?}", host.id());

    let device = resolve_device(&host, device_query)?;

    log::info!("Chosen device: {:?}", device);

    let max_pair_idx = deck_routing.iter().copied().max().unwrap_or(0);
    let min_channels_required = ((max_pair_idx + 1) * 2) as u16;

    let all_configs: Vec<_> = device
        .supported_output_configs()
        .context("Failed to query device supported output configs")?
        .collect();

    let mut valid_configs: Vec<_> = all_configs
        .iter()
        .cloned()
        .filter(|config| {
            // Must support F32 sample format
            if config.sample_format() != SampleFormat::F32 {
                return false;
            }

            // Must have enough channels and be an even pair count
            let channels = config.channels();
            if channels < min_channels_required || channels % 2 != 0 {
                return false;
            }

            // Target SAMPLE_RATE must fall within supported range
            if !(config.min_sample_rate() <= SAMPLE_RATE && SAMPLE_RATE <= config.max_sample_rate())
            {
                return false;
            }

            // Requested buffer frame size must fall within supported range
            match config.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    buffer_frames_n >= *min && buffer_frames_n <= *max
                }
                cpal::SupportedBufferSize::Unknown => true,
            }
        })
        .collect();

    if valid_configs.is_empty() {
        let available_str = if all_configs.is_empty() {
            "  (none reported by driver)".to_string()
        } else {
            all_configs
                .iter()
                .map(|c| {
                    format!(
                        "  - channels: {}, format: {:?}, sample_rate: {}-{} Hz, buffer: {:?}",
                        c.channels(),
                        c.sample_format(),
                        c.min_sample_rate(),
                        c.max_sample_rate(),
                        c.buffer_size()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        bail!(
            "Device \"{device}\" does not support required config:\n\
        \x20 Required: >= {min_channels_required} channels (even, covering stereo pair {max_pair_idx}), F32 format, rate {SAMPLE_RATE:?}, buffer {buffer_frames_n} frames\n\n\
        Available device configurations:\n{available_str}"
        );
    }

    // 2. Select the optimal configuration (e.g. smallest suitable channel count >= min_channels_required)
    valid_configs.sort_by_key(|config| config.channels());
    let chosen_range = valid_configs.remove(0);

    let device_channels = chosen_range.channels() as usize;

    let chosen_config = StreamConfig {
        channels: chosen_range.channels(),
        sample_rate: SAMPLE_RATE,
        buffer_size: BufferSize::Fixed(buffer_frames_n),
    };

    log::info!(
        "Selected matching output config: channels={}, sample_rate={:?}, buffer_size={} frames",
        chosen_config.channels,
        chosen_config.sample_rate,
        buffer_frames_n
    );

    // 3. Validate routing against the physical channels offered by the chosen config
    let deck_routing = DeckRouting::try_new(deck_routing, device_channels)?;

    // 4. Instantiate processor poller and build audio stream
    let processors = audio_processor_handles.map(PlatterAudioProcessor::new);
    let mut samples_poller = SamplesPoller::new(buffer_frames_n as usize, processors, deck_routing);

    let stream = device.build_output_stream(
        chosen_config,
        move |data: &mut [f32], _| {
            samples_poller.write_frames(data);
        },
        move |err| {
            log::error!("Audio stream execution error: {err}");
        },
        None,
    )?;

    stream.play()?;
    Ok(stream)
}
