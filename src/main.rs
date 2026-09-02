mod app;

use std::{fs::OpenOptions, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use log::info;

use crate::app::start;

/// Which device drives the decks.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum InputKind {
    /// SDL window: mouse scratching plus the keyboard bindings
    Touchpad,
    /// MIDI controller (DDJ-FLX4): jog wheels, transport, tempo faders
    Midi,
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Turntable Scratch Engine CLI", long_about = None)]
struct Cli {
    /// Path to log file
    #[arg(
        long,
        global = true,
        default_value = "/home/sancho20021/spw/localdeck/turntable.log"
    )]
    log_file: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List MIDI input ports and exit
    ListMidi,

    /// Run the turntable application
    Run {
        /// Input device driving the decks
        #[arg(short('I'), long, value_enum, default_value_t = InputKind::Touchpad)]
        input: InputKind,

        /// MIDI port index or name substring (default: the first DDJ port)
        #[arg(long)]
        midi_port: Option<String>,

        /// Tempo fader range as a fraction, e.g. 0.08 for +/-8%
        #[arg(long, default_value_t = 0.08)]
        pitch_range: f64,

        /// Stereo pair assignments for each deck (e.g., "0" for 1 deck, "0,1" for 2 decks, "1,0" for 2 decks with swapped order, etc)
        #[arg(
            short('r'),
            long = "routing",
            value_delimiter = ',',
            default_value = "0"
        )]
        routing: Vec<usize>,

        /// Audio output device name or substring query
        #[arg(short('D'), long)]
        device: Option<String>,

        /// Audio callback buffer size
        #[arg(short, long, default_value_t = 256)]
        buffer: u32,

        /// Scratch sensitivity factor, applied to whichever input is in use
        #[arg(short('t'), long, default_value_t = 1.)]
        sensitivity: f64,

        /// Motor inertia parameter in seconds
        #[arg(short('i'), long, default_value_t = 0.5)]
        motor_inertia: f64,

        /// Nudge / Pitch bend responsiveness
        #[arg(short('n'), long, default_value_t = 1., allow_negative_numbers = true)]
        nudge: f32,
    },
}

/// Puts panics in the log file, where they survive the TUI.
///
/// The TUI holds the terminal in raw mode, so the default handler's message is
/// swallowed. Also catches a panic that aborts across the audio callback's C
/// boundary, since the hook runs before the abort.
fn install_panic_logger() {
    let default_hook = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let location = match info.location() {
            Some(l) => format!("{}:{}:{}", l.file(), l.line(), l.column()),
            None => "<unknown location>".to_string(),
        };
        let message = info.payload_as_str().unwrap_or("<non-string payload>");

        // Logged first, so the log has the panic even if the restore below hangs.
        log::error!(
            "PANIC on thread '{thread_name}' at {location}: {message}\nbacktrace:\n{}",
            std::backtrace::Backtrace::force_capture()
        );

        // Leaving raw mode makes the default handler's output readable.
        ratatui::restore();
        default_hook(info);
    }));
}

fn main() {
    let args = Cli::parse();

    if let Some(parent) = args.log_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // 3. Open or create the target log file
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log_file)
        .unwrap_or_else(|err| panic!("Failed to open log file at {:?}: {}", args.log_file, err));

    // 4. Initialize env_logger with target pipe
    env_logger::builder()
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .init();

    install_panic_logger();

    info!("Initialized logging to {:?}", args.log_file);

    match &args.command {
        Commands::ListMidi => match turntable_lib::midi::list_ports() {
            Ok(ports) if ports.is_empty() => println!("No MIDI input ports found"),
            Ok(ports) => {
                println!("MIDI input ports:");
                for (i, name) in ports.iter().enumerate() {
                    println!("  [{i}] {name}");
                }
            }
            Err(e) => {
                eprintln!("Cannot list MIDI ports: {e}");
                std::process::exit(1);
            }
        },

        Commands::Run {
            input,
            midi_port,
            pitch_range,
            routing,
            device,
            buffer,
            sensitivity,
            motor_inertia,
            nudge,
        } => {
            log::info!("Starting Turntable: {:?}", args.command);
            start(app::Options {
                input: *input,
                midi_port: midi_port.as_deref(),
                pitch_range: *pitch_range,
                deck_routing: routing,
                device_query: device.as_deref(),
                motor_inertia_secs: *motor_inertia,
                sensitivity: *sensitivity,
                buffer_frames_n: *buffer,
                nudge_responsiveness: *nudge,
            });
        }
    }
}
