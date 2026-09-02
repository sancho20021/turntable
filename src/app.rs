use std::{
    array,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use cpal::{
    BufferSize, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam::channel::{Receiver, Sender, bounded};

use crate::InputKind;
use turntable_lib::{
    audio_health::{self, AudioHealth, HealthRecorder},
    deck_controller::{self, AppStatus, DeckController, DeckId},
    decoder::SAMPLE_RATE,
    input_event::{AppEvent, DeckEvent, InputEvent},
    input_profile::InputProfile,
    midi,
    platter_audio_processor::{AudioProcessorHandles, PlatterAudioProcessor},
    platter_driver::PlatterDriver,
    ratatui::spawn_tui_thread,
    samples_poller::{DeckRouting, SamplesPoller},
    sdl_input::SdlInputMapper,
    telemetry,
    tray::{self, TrayCommand, TrayState},
    utils::{log_try_send, unzip_array4},
};

/// Everything the app was asked to do, as parsed from the command line.
pub struct Options<'a> {
    pub input: InputKind,
    pub midi_port: Option<&'a str>,
    /// tempo fader range as a fraction, 0.08 = +/-8%
    pub pitch_range: f64,
    pub deck_routing: &'a [usize],
    pub device_query: Option<&'a str>,
    pub motor_inertia_secs: f64,
    /// scratch sensitivity factor, applied to whichever input is in use
    pub sensitivity: f64,
    /// audio buffer in frames
    pub buffer_frames_n: u32,
    /// nudge / pitch bend responsiveness factor, applied to whichever input is in use
    pub nudge_responsiveness: f32,
}

/// Turns the runtime deck count into the const generic the engine is built on.
macro_rules! dispatch_app {
    ($decks:expr, $options:expr) => {{
        let routing: [usize; $decks] = $options
            .deck_routing
            .try_into()
            .expect("Routing slice length does not match deck count");
        run_app::<$decks>(routing, &$options);
    }};
}

/// Main app loop entrypoint.
pub fn start(options: Options) {
    match options.deck_routing.len() {
        1 => dispatch_app!(1, options),
        2 => dispatch_app!(2, options),
        3 => dispatch_app!(3, options),
        4 => dispatch_app!(4, options),
        _ => panic!("Maximum 4 decks supported"),
    }
}

fn run_app<const DECKS: usize>(deck_routing: [usize; DECKS], options: &Options) {
    let app_status = AppStatus::new();

    // preparing records and loading them onto decks
    let (tray_snd, tray_rcv) = bounded(3);
    let tray_state = Arc::new(RwLock::new(TrayState::Empty));
    let shutdown = Arc::new(AtomicBool::new(false));

    // One input unit is a touchpad pixel or a jog wheel tick depending on what
    // is driving the decks, which is the only thing the engine needs to be told
    // about the difference.
    let input_profile = match options.input {
        InputKind::Touchpad => {
            InputProfile::touchpad(options.sensitivity, options.nudge_responsiveness)
        }
        InputKind::Midi => {
            InputProfile::jog_wheel(options.sensitivity, options.nudge_responsiveness)
        }
    };

    // Only the keyboard has a notion of an active deck; a MIDI controller names
    // its deck in every message.
    let active_deck = match options.input {
        InputKind::Touchpad => Some(Arc::new(AtomicUsize::new(0))),
        InputKind::Midi => None,
    };

    // One health struct for the whole stream, with a slot per deck. The audio
    // thread only ever bumps atomics in it; the monitor thread does the logging.
    let health = AudioHealth::new(options.buffer_frames_n, SAMPLE_RATE, DECKS);
    let (health_recorder, health_events) = audio_health::new_recorder(Arc::clone(&health));

    let deck_tuples = std::array::from_fn(|deck_idx| {
        deck_controller::new_deck(
            deck_idx,
            input_profile.clone(),
            options.motor_inertia_secs,
            tray_snd.clone(),
            Arc::clone(&shutdown),
            options.buffer_frames_n as usize,
            health.deck(deck_idx),
        )
    });

    let (controllers, drivers, audio_processor_handles, deck_slots) = unzip_array4(deck_tuples);

    let started_drivers = drivers.map(PlatterDriver::start);

    let stream = start_deck(
        options.buffer_frames_n,
        audio_processor_handles,
        deck_routing,
        options.device_query,
        health_recorder,
    )
    .unwrap();

    let health_monitor = audio_health::spawn_monitor(health, health_events, Arc::clone(&shutdown));

    let tray = tray::start(
        tray_rcv,
        deck_slots,
        Arc::clone(&tray_state),
        app_status.clone(),
        Arc::clone(&shutdown),
    );

    // Input sources produce events; the dispatcher applies them. Keeping those
    // apart is what lets a source live on a thread it does not own - SDL's pump
    // must stay on the thread that initialised video, the TUI is busy drawing,
    // and a MIDI callback runs on a driver thread we are only handed.
    let (events_snd, events_rcv) = bounded(EVENT_QUEUE_LEN);

    // Spawn polling TUI thread, which doubles as the drag and drop source
    let tui_handle = spawn_tui_thread::<DECKS>(
        active_deck.clone(),
        array::from_fn(|i| controllers[i].get_state()),
        array::from_fn(|i| controllers[i].get_platter()),
        Arc::clone(&tray_state),
        app_status.clone(),
        events_snd.clone(),
        Arc::clone(&shutdown),
    );

    let dispatcher = spawn_dispatcher(
        events_rcv,
        controllers,
        tray_snd.clone(),
        Arc::clone(&shutdown),
    );

    // Whichever source owns the main thread blocks here until the app stops.
    match options.input {
        InputKind::Touchpad => run_sdl_source::<DECKS>(
            active_deck.expect("touchpad input always has an active deck"),
            app_status.clone(),
            &events_snd,
            &dispatcher,
        ),
        InputKind::Midi => run_midi_source(options, &events_snd, &dispatcher),
    }

    // 4. Teardown & Thread Joining
    log::info!("Stopping the app");
    drop(stream);
    shutdown.store(true, Ordering::Relaxed);

    let controllers = match dispatcher.join() {
        Ok(controllers) => Some(controllers),
        Err(e) => {
            log::error!("Dispatcher panicked: {e:?}");
            None
        }
    };

    if let Err(_) = tray.join() {
        log::error!("Record tray panicked");
    }

    // Join all platter driver threads
    let platter_drivers = started_drivers.map(|handle| match handle.join() {
        Ok(driver) => Some(driver),
        Err(e) => {
            log::error!("Platter driver panicked: {e:?}");
            None
        }
    });

    if let Err(_) = tui_handle.join() {
        log::error!("TUI thread panicked");
    }

    // Joined last of the workers: it reports the session totals on the way out.
    if let Err(_) = health_monitor.join() {
        log::error!("Audio health monitor panicked");
    }

    // 5. Consolidate telemetry tracers across controllers and driver threads
    let mut tracers = Vec::with_capacity(DECKS * 2);
    for controller in controllers.into_iter().flatten() {
        tracers.push(controller.tracer);
    }
    for driver in platter_drivers.into_iter().flatten() {
        tracers.push(driver.tracer);
    }

    telemetry::save_traces_to_file(tracers, "trace.csv").expect("Failed to save telemetry");
}

/// Runs the SDL window as the input source, returning when the app should stop.
///
/// SDL is only initialised here, so a MIDI-driven run opens no window at all.
fn run_sdl_source<const DECKS: usize>(
    active_deck: Arc<AtomicUsize>,
    app_status: AppStatus,
    events: &Sender<InputEvent>,
    dispatcher: &JoinHandle<[DeckController; DECKS]>,
) {
    let sdl = sdl2::init().unwrap();
    let video = sdl.video().unwrap();

    let _window = video
        .window("scratch input", 600, 300)
        .position_centered()
        .build()
        .unwrap();

    let mut pump = sdl.event_pump().unwrap();
    let mut mapper = SdlInputMapper::<DECKS>::new(active_deck, app_status);

    // The wait has a timeout only so that a quit raised somewhere else - Ctrl-C
    // in the TUI - is noticed within it. Events arriving on the pump still wake
    // it immediately, so input latency is untouched.
    while !dispatcher.is_finished() {
        let Some(event) = pump.wait_event_timeout(QUIT_POLL_MS) else {
            continue;
        };

        // Deliberately not `event.timestamp()`: SDL2 stamps events with
        // `SDL_GetTicks()`, in whole milliseconds, and mouse motion arrives
        // every 1-8 ms - quantising the gaps to a millisecond would cost more
        // than the scheduling jitter it saves. The MIDI source does use its
        // device's stamp, because ALSA's is microseconds; see
        // [`turntable_lib::clock_sync`] for when that trade is worth making.
        let now = Instant::now();
        let Some(input_event) = mapper.to_input_event(event, now) else {
            continue;
        };

        if matches!(input_event, InputEvent::App(AppEvent::Quit)) {
            break;
        }

        if events.send(input_event).is_err() {
            log::error!("Dispatcher is gone, stopping input");
            break;
        }
    }
}

/// Opens the MIDI controller and parks until the app should stop.
///
/// There is nothing for the main thread to do: midir runs the callback on its
/// own driver thread, and quitting comes from the TUI.
fn run_midi_source<const DECKS: usize>(
    options: &Options,
    events: &Sender<InputEvent>,
    dispatcher: &JoinHandle<[DeckController; DECKS]>,
) {
    let _connection = match midi::start(options.midi_port, options.pitch_range, events.clone()) {
        // held for the whole run: dropping it closes the port
        Ok(connection) => connection,
        Err(e) => {
            log::error!("Cannot start MIDI input: {e}");
            eprintln!("Cannot start MIDI input: {e}");
            return;
        }
    };

    while !dispatcher.is_finished() {
        std::thread::sleep(Duration::from_millis(QUIT_POLL_MS as u64));
    }
}

/// How long the SDL pump blocks before checking whether the app is stopping.
/// Only bounds how fast a quit from another source is noticed; events arriving
/// on the pump wake it straight away.
const QUIT_POLL_MS: u32 = 100;

/// Input events buffered between the sources and the dispatcher. Deep enough
/// that a burst of scratch motion never has to wait; sources block rather than
/// drop if it ever fills, since a lost scratch position is a heard glitch.
const EVENT_QUEUE_LEN: usize = 1024;

/// Applies input events to the decks, and owns the controllers while doing so.
///
/// Returns them on join, so teardown can still collect their telemetry.
fn spawn_dispatcher<const DECKS: usize>(
    events: Receiver<InputEvent>,
    mut controllers: [DeckController; DECKS],
    tray: Sender<TrayCommand>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<[DeckController; DECKS]> {
    std::thread::spawn(move || {
        while !shutdown.load(Ordering::Relaxed) {
            let event = match events.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            };

            match event {
                // Stopping is the main thread's job: it drops the audio stream
                // before the platter threads. Returning is how it hears about it,
                // whichever source the quit came from.
                InputEvent::App(AppEvent::Quit) => break,

                // Only prepares the record. Which deck it ends up on is decided
                // later, by whoever loads it: Enter here, a LOAD button on MIDI.
                InputEvent::App(AppEvent::PrepareRecord(path)) => {
                    log_try_send(&tray, TrayCommand::PrepareRecord { path }, "prepare record")
                }

                InputEvent::Deck(deck_idx, event) => {
                    dispatch_to_deck(&mut controllers, deck_idx, event)
                }
            }
        }

        log::info!("Dispatcher stopped");
        controllers
    })
}

/// Hands one deck event to its controller, complaining if the deck does not exist.
fn dispatch_to_deck<const DECKS: usize>(
    controllers: &mut [DeckController; DECKS],
    deck_idx: DeckId,
    event: DeckEvent,
) {
    match controllers.get_mut(deck_idx) {
        Some(controller) => controller.handle_deck_event(event),
        None => log::warn!("Received event for non-existent deck index: {deck_idx}"),
    }
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
    health: HealthRecorder,
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
    let mut samples_poller =
        SamplesPoller::new(buffer_frames_n as usize, processors, deck_routing, health);

    let stream = device.build_output_stream(
        chosen_config,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            // The device clock, not ours: the only source that knows how many
            // callbacks the graph made while we were not looking.
            let callback_nanos = info.timestamp().callback.as_nanos() as u64;
            samples_poller.write_frames(data, callback_nanos);
        },
        move |err| {
            log::error!("Audio stream execution error: {err}");
        },
        None,
    )?;

    stream.play()?;
    Ok(stream)
}
