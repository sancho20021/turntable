//! The set recorder: a `pw-record` child writing the mixer's master return to
//! a WAV while the DJ asks for it.
//!
//! turntable never talks to PipeWire itself. It hands `pw-record` its target in
//! argv, stops it with SIGINT, and learns how it is going from its exit, its
//! stderr, and the file it writes.
//!
//! Which source gets recorded is settled at spawn. The target is named exactly,
//! and WirePlumber is told not to fall back to the default source:
//! `node.dont-reconnect` on 0.4 (`policy-node.lua`), `node.dont-fallback` on
//! 0.5. A target that is missing, is not a source, or vanishes mid-set gets
//! `target not found` and `pw-record` exits.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStderr, Command, Stdio},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use chrono::NaiveDateTime;
use crossbeam::channel::{Receiver, RecvTimeoutError};
use rustix::{
    io::Errno,
    process::{Pid, Signal, getppid, kill_process, set_parent_process_death_signal},
};

use crate::{decoder::SAMPLE_RATE, notices::Notices};

/// What libsndfile writes ahead of the first sample of a 16-bit stereo WAV.
const WAV_HEADER_BYTES: u64 = 44;

/// Past this, `pw-record` is wedged. SIGKILL then costs the header's length
/// field, not the audio.
const STOP_GRACE: Duration = Duration::from_secs(2);

const TICK: Duration = Duration::from_millis(100);

pub struct RecorderConfig {
    /// exact PipeWire node name of the capture source
    pub target: String,
    /// where the WAVs go, created if missing
    pub dir: PathBuf,
}

pub enum RecorderCommand {
    Start,
    Stop,
}

/// Published for the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderState {
    Idle,
    /// `pw-record` is running and nothing has reached the file yet.
    Starting {
        path: PathBuf,
    },
    /// `since` is when the first audio reached the file.
    Recording {
        path: PathBuf,
        since: Instant,
    },
    /// SIGINT sent, waiting for `pw-record` to finish the file.
    Stopping {
        path: PathBuf,
    },
}

/// Spawns the recorder thread.
///
/// It is also the thread that spawns `pw-record`, and PDEATHSIG ties the child
/// to it, so it must live until shutdown. See [`spawn_with_deathsig`].
pub fn start(
    config: RecorderConfig,
    commands: Receiver<RecorderCommand>,
    state: Arc<RwLock<RecorderState>>,
    notices: Notices,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let target = config.target;
    let recorder = Recorder::new(
        config.dir,
        Box::new(move |path| pw_record(&target, path)),
        state,
        notices,
        STOP_GRACE,
    );
    std::thread::spawn(move || recorder.run(commands, shutdown))
}

/// Loudest sample of 16-bit little-endian PCM, in dBFS.
///
/// `Some` when any sample is non-zero. An ADC always has some noise, so all
/// zeros means there is no signal path at all. A trailing odd byte is ignored.
pub fn peak_dbfs(pcm: &[u8]) -> Option<f32> {
    let peak = pcm
        .chunks_exact(2)
        .map(|sample| i32::from(i16::from_le_bytes([sample[0], sample[1]])).abs())
        .max()
        .filter(|&peak| peak > 0)?;
    Some(20. * (peak as f32 / 32768.).log10())
}

fn record_args(target: &str, path: &Path) -> Vec<OsString> {
    vec![
        format!("--target={target}").into(),
        format!("--rate={SAMPLE_RATE}").into(),
        "--channels=2".into(),
        "--format=s16".into(),
        "-P".into(),
        "{ node.name = turntable-recorder node.dont-reconnect = true node.dont-fallback = true }"
            .into(),
        path.into(),
    ]
}

fn pw_record(target: &str, path: &Path) -> Command {
    let mut command = Command::new("pw-record");
    command.args(record_args(target, path));
    command
}

fn output_path(dir: &Path, now: NaiveDateTime) -> PathBuf {
    dir.join(format!("mix_{}.wav", now.format("%Y-%m-%d_%H-%M-%S")))
}

/// The part of a `pw-record` error line worth showing the DJ:
/// `remote error: id=2 seq:7 res:-2 (No such file or directory): target not found`
/// becomes `target not found`.
fn reason(line: &str) -> &str {
    line.rsplit(": ").next().unwrap_or(line)
}

/// `mm:ss`, or `h:mm:ss` from an hour up.
fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs / 3600 {
        0 => format!("{:02}:{:02}", secs / 60, secs % 60),
        hours => format!("{hours}:{:02}:{:02}", secs / 60 % 60, secs % 60),
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

/// Spawns `command` with the kernel set to send it SIGINT when the calling
/// thread exits. That covers a turntable that panics or is killed outright,
/// where no code of ours gets to stop the child. SIGINT lets `pw-record` finish
/// the WAV.
///
/// The "parent" of PR_SET_PDEATHSIG is the thread that spawned the child, not
/// the process (prctl(2)).
fn spawn_with_deathsig(command: &mut Command) -> std::io::Result<Child> {
    let parent = Pid::from_raw(std::process::id() as i32);

    // SAFETY: the closure runs in the forked child, which may only do
    // async-signal-safe work. It makes two syscalls and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            set_parent_process_death_signal(Some(Signal::INT))?;
            // The parent may have died before this ran, and then no signal
            // will ever come.
            if getppid() != parent {
                return Err(Errno::SRCH.into());
            }
            Ok(())
        })
    };

    command.spawn()
}

#[derive(Debug, PartialEq, Eq)]
enum Stopped {
    Clean,
    Killed,
}

/// SIGINT, then SIGKILL if the child is still there after `grace`.
fn stop_child(child: &mut Child, grace: Duration) -> Stopped {
    if let Err(e) = kill_process(Pid::from_child(child), Signal::INT) {
        log::warn!("Cannot send SIGINT to pw-record: {e}");
    }

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        match child.try_wait() {
            // An error means something else reaped the child. It is gone, and
            // its pid may already belong to another process, so it must not
            // be signalled again.
            Ok(Some(_)) | Err(_) => return Stopped::Clean,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    Stopped::Killed
}

/// Copies the child's stderr into the log as it arrives, so the pipe never
/// fills and blocks `pw-record`. The thread ends when the child closes stderr,
/// and returns the last line, which says why it exited.
fn drain_stderr(stderr: ChildStderr) -> JoinHandle<Option<String>> {
    std::thread::spawn(move || {
        let mut last = None;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            log::warn!("pw-record: {line}");
            last = Some(line);
        }
        last
    })
}

/// A running `pw-record`: the child, the thread draining its stderr, and the
/// file it writes. Lives from spawn to reap, across Starting, Recording and
/// Stopping.
struct Capture {
    child: Child,
    stderr: JoinHandle<Option<String>>,
    path: PathBuf,
}

/// Builds the recording command for a path. `pw-record` in production, a
/// stand-in in the tests.
type CommandFor = Box<dyn Fn(&Path) -> Command + Send>;

struct Recorder {
    dir: PathBuf,
    command_for: CommandFor,
    capture: Option<Capture>,
    state: RecorderState,
    published: Arc<RwLock<RecorderState>>,
    notices: Notices,
    grace: Duration,
}

impl Recorder {
    fn new(
        dir: PathBuf,
        command_for: CommandFor,
        published: Arc<RwLock<RecorderState>>,
        notices: Notices,
        grace: Duration,
    ) -> Self {
        let recorder = Self {
            dir,
            command_for,
            capture: None,
            state: RecorderState::Idle,
            published,
            notices,
            grace,
        };
        recorder.publish_current();
        recorder
    }

    fn run(mut self, commands: Receiver<RecorderCommand>, shutdown: Arc<AtomicBool>) {
        while !shutdown.load(Ordering::Relaxed) {
            match commands.recv_timeout(TICK) {
                Ok(RecorderCommand::Start) => self.start(),
                Ok(RecorderCommand::Stop) => self.stop(),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.watch();
        }

        self.stop();
        log::info!("Recorder stopped");
    }

    fn start(&mut self) {
        if self.capture.is_some() {
            return;
        }

        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            self.notices
                .error(format!("Cannot record into {}: {e}", self.dir.display()));
            return;
        }

        let path = output_path(&self.dir, chrono::Local::now().naive_local());
        let mut command = (self.command_for)(&path);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = match spawn_with_deathsig(&mut command) {
            Ok(child) => child,
            Err(e) => {
                self.notices.error(format!("Cannot start pw-record: {e}"));
                return;
            }
        };

        let stderr = drain_stderr(child.stderr.take().expect("stderr is piped"));
        log::info!("Recording to {}", path.display());
        self.publish(RecorderState::Starting { path: path.clone() });
        self.capture = Some(Capture {
            child,
            stderr,
            path,
        });
    }

    /// Notices a `pw-record` that exited without being asked to, and the first
    /// audio reaching the file.
    fn watch(&mut self) {
        let Some(capture) = self.capture.as_mut() else {
            return;
        };

        // pw-record exits 1 after a clean SIGINT too, so the status says
        // nothing: any exit turntable did not ask for is a failure. An error
        // means something else reaped the child, so it is gone as well.
        let exit = match capture.child.try_wait() {
            Ok(None) => {
                if let RecorderState::Starting { path } = &self.state
                    && file_size(path) > WAV_HEADER_BYTES
                {
                    let path = path.clone();
                    self.publish(RecorderState::Recording {
                        path,
                        since: Instant::now(),
                    });
                }
                return;
            }
            Ok(Some(status)) => status.to_string(),
            Err(e) => e.to_string(),
        };

        let capture = self.capture.take().expect("checked above");
        let line = capture.stderr.join().ok().flatten();
        log::error!(
            "pw-record exited on its own ({exit}) while writing {}",
            capture.path.display()
        );
        let why = line.as_deref().map_or("pw-record exited", reason);

        let message = match &self.state {
            RecorderState::Recording { since, .. } => {
                format!("Recording stopped after {}: {why}", clock(since.elapsed()))
            }
            _ => format!("Cannot record: {why}"),
        };
        self.notices.error(message);
        self.publish(RecorderState::Idle);
    }

    fn stop(&mut self) {
        let Some(mut capture) = self.capture.take() else {
            return;
        };

        let length = match &self.state {
            RecorderState::Recording { since, .. } => clock(since.elapsed()),
            _ => clock(Duration::ZERO),
        };
        self.publish(RecorderState::Stopping {
            path: capture.path.clone(),
        });

        let stopped = stop_child(&mut capture.child, self.grace);
        let _ = capture.stderr.join();

        let name = capture.path.display();
        match stopped {
            Stopped::Clean => log::info!("Saved {name} ({length})"),
            Stopped::Killed => {
                log::error!("pw-record ignored SIGINT and was killed; {name} has no final header");
                self.notices.warn(format!(
                    "pw-record had to be killed, {name} may need repairing"
                ));
            }
        }
        self.publish(RecorderState::Idle);
    }

    fn publish(&mut self, state: RecorderState) {
        self.state = state;
        self.publish_current();
    }

    fn publish_current(&self) {
        match self.published.write() {
            Ok(mut published) => *published = self.state.clone(),
            Err(_) => log::error!("cannot publish the recorder state, lock poisoned"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::process::ExitStatusExt, process::ExitStatus, sync::atomic::AtomicUsize};

    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    /// Polls `done` until it holds or `limit` runs out.
    fn eventually(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    fn exited_within(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
        let mut status = None;
        eventually(limit, || {
            status = child.try_wait().ok().flatten();
            status.is_some()
        });
        status
    }

    /// A directory of its own per test, so tests can run in parallel.
    fn scratch_dir() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "turntable-recorder-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    struct Harness {
        recorder: Recorder,
        notices: Notices,
        dir: PathBuf,
    }

    impl Harness {
        fn new(command_for: CommandFor) -> Self {
            let notices = Notices::new();
            let dir = scratch_dir();
            let recorder = Recorder::new(
                dir.clone(),
                command_for,
                Arc::new(RwLock::new(RecorderState::Idle)),
                notices.clone(),
                Duration::from_millis(200),
            );
            Self {
                recorder,
                notices,
                dir,
            }
        }

        /// The recorder with `script` standing in for `pw-record`. The script
        /// gets the WAV path as `$1`.
        fn with_script(script: &'static str) -> Self {
            Self::new(Box::new(move |path| {
                let mut command = sh(script);
                command.arg("sh").arg(path);
                command
            }))
        }

        fn state(&self) -> RecorderState {
            self.recorder.state.clone()
        }

        fn watch_until(&mut self, limit: Duration, done: impl Fn(&RecorderState) -> bool) -> bool {
            eventually(limit, || {
                self.recorder.watch();
                done(&self.recorder.state)
            })
        }

        fn notice(&self) -> String {
            self.notices
                .current()
                .map(|notice| notice.message)
                .unwrap_or_default()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.recorder.stop();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn is_recording(state: &RecorderState) -> bool {
        matches!(state, RecorderState::Recording { .. })
    }

    // --- pure ---

    #[test]
    fn the_command_line_pins_the_target() {
        let args = record_args("alsa_input.xone", Path::new("/rec/mix.wav"));
        let expected: Vec<OsString> = [
            "--target=alsa_input.xone",
            "--rate=48000",
            "--channels=2",
            "--format=s16",
            "-P",
            "{ node.name = turntable-recorder node.dont-reconnect = true node.dont-fallback = true }",
            "/rec/mix.wav",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn peaks_are_measured_against_full_scale() {
        assert_eq!(peak_dbfs(&[0; 64]), None);
        assert_eq!(peak_dbfs(&[]), None);

        let peak_of = |sample: i16| {
            let mut pcm = vec![0; 8];
            pcm.extend(sample.to_le_bytes());
            peak_dbfs(&pcm).expect("not silent")
        };
        assert!(peak_of(i16::MAX).abs() < 0.001);
        assert!((peak_of(16384) + 6.0206).abs() < 0.001);
        assert!((peak_of(-16384) + 6.0206).abs() < 0.001);
        assert_eq!(peak_of(i16::MIN), 0.);
    }

    // --- processes, no audio ---

    #[test]
    fn a_child_ignoring_sigint_is_killed_after_the_grace() {
        let mut command = sh("trap '' INT; echo ready; exec sleep 30");
        command.stdout(Stdio::piped());
        let mut child = spawn_with_deathsig(&mut command).expect("spawns");

        // A SIGINT that lands before the trap is set kills the shell outright.
        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("piped"))
            .read_line(&mut ready)
            .expect("ready line");

        let started = Instant::now();
        assert_eq!(
            stop_child(&mut child, Duration::from_millis(200)),
            Stopped::Killed
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    /// The thread that spawned the child ends while the test keeps running, so
    /// this also shows the signal follows the thread, not the process.
    #[test]
    fn the_child_gets_sigint_when_its_spawning_thread_ends() {
        let mut child = std::thread::spawn(|| spawn_with_deathsig(&mut sh("exec sleep 30")))
            .join()
            .expect("spawning thread")
            .expect("spawns");

        let status = exited_within(&mut child, Duration::from_secs(2));
        if status.is_none() {
            let _ = child.kill();
        }
        assert_eq!(status.and_then(|s| s.signal()), Some(Signal::INT.as_raw()));
    }

    #[test]
    fn stderr_is_drained_and_its_last_line_kept() {
        let mut command = sh("yes x | head -c 200000 >&2; echo 'last words' >&2");
        command.stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawns");
        let stderr = drain_stderr(child.stderr.take().expect("piped"));

        // 200 KB is past a pipe's capacity: undrained, the child would block.
        assert!(exited_within(&mut child, Duration::from_secs(5)).is_some());
        assert_eq!(
            stderr.join().expect("drain thread"),
            Some("last words".into())
        );
    }

    #[test]
    fn a_header_alone_is_still_starting() {
        let mut h = Harness::with_script("head -c 44 /dev/zero > \"$1\"; exec sleep 30");
        h.recorder.start();
        assert!(!h.watch_until(Duration::from_millis(500), is_recording));
        assert!(matches!(h.state(), RecorderState::Starting { .. }));
    }

    #[test]
    fn a_missing_program_is_reported() {
        let mut h = Harness::new(Box::new(|_| Command::new("turntable-no-such-program")));
        h.recorder.start();
        assert_eq!(h.state(), RecorderState::Idle);
        assert!(
            h.notice().starts_with("Cannot start pw-record"),
            "{}",
            h.notice()
        );
    }

    #[test]
    fn shutdown_stops_the_recording() {
        let (commands, receive) = crossbeam::channel::bounded(1);
        let state = Arc::new(RwLock::new(RecorderState::Idle));
        let shutdown = Arc::new(AtomicBool::new(false));
        let dir = scratch_dir();

        let recorder = Recorder::new(
            dir.clone(),
            Box::new(|path| {
                let mut command = sh("head -c 1000 /dev/zero > \"$1\"; exec sleep 30");
                command.arg("sh").arg(path);
                command
            }),
            Arc::clone(&state),
            Notices::new(),
            Duration::from_millis(200),
        );
        let thread = {
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || recorder.run(receive, shutdown))
        };

        commands
            .send(RecorderCommand::Start)
            .expect("recorder alive");
        let read = || state.read().expect("lock").clone();
        assert!(eventually(Duration::from_secs(2), || is_recording(&read())));

        shutdown.store(true, Ordering::Relaxed);
        thread.join().expect("recorder thread");
        assert_eq!(read(), RecorderState::Idle);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- real PipeWire, virtual source ---

    /// A PipeWire source that turntable can record from, optionally playing a
    /// 1 kHz tone at half scale. A loopback's sink side takes the tone; its
    /// source side is what gets recorded.
    ///
    /// Both sides are at `priority.session = 0`, so neither becomes a default
    /// device. A `pw-loopback` without these props links the mic to the
    /// speakers instead.
    struct VirtualSource {
        name: String,
        loopback: Child,
        player: Option<Child>,
        dir: PathBuf,
    }

    impl VirtualSource {
        fn new(tone: bool) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                "turntable-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            );
            let props = |class: &str, node: &str| {
                format!("media.class={class} node.name={node} priority.session=0 priority.driver=0")
            };

            let loopback = spawn_with_deathsig(
                Command::new("pw-loopback")
                    .arg(format!("--name={name}"))
                    .arg(format!(
                        "--capture-props={}",
                        props("Audio/Sink", &format!("{name}-in"))
                    ))
                    .arg(format!(
                        "--playback-props={}",
                        props("Audio/Source", &format!("{name}-out"))
                    ))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
            )
            .expect("pw-loopback spawns");

            let dir = scratch_dir();
            std::fs::create_dir_all(&dir).expect("scratch dir");

            let mut source = Self {
                name,
                loopback,
                player: None,
                dir,
            };
            source.wait_for_ports(&format!("{}-out:", source.name));

            if tone {
                let wav = source.dir.join("tone.wav");
                write_tone(&wav, 10);
                source.player = Some(
                    spawn_with_deathsig(
                        Command::new("pw-play")
                            .arg(format!("--target={}-in", source.name))
                            .arg(&wav)
                            .stdout(Stdio::null())
                            .stderr(Stdio::null()),
                    )
                    .expect("pw-play spawns"),
                );
            }
            source
        }

        fn target(&self) -> String {
            format!("{}-out", self.name)
        }

        /// The recorder would otherwise race the node's creation and be
        /// told `target not found`.
        fn wait_for_ports(&self, prefix: &str) {
            let up = eventually(Duration::from_secs(5), || {
                Command::new("pw-link")
                    .arg("-o")
                    .output()
                    .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(prefix))
            });
            assert!(up, "{prefix} never appeared; is PipeWire running?");
        }
    }

    impl Drop for VirtualSource {
        fn drop(&mut self) {
            for child in self.player.iter_mut().chain([&mut self.loopback]) {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A stereo 16-bit WAV of a 1 kHz sine at half scale, -6.02 dBFS.
    fn write_tone(path: &Path, secs: u32) {
        let frames = SAMPLE_RATE * secs;
        let data_len = frames * 4;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend(b"RIFF");
        wav.extend((36 + data_len).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(SAMPLE_RATE.to_le_bytes());
        wav.extend((SAMPLE_RATE * 4).to_le_bytes());
        wav.extend(4u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(data_len.to_le_bytes());
        for i in 0..frames {
            let phase = 2. * std::f64::consts::PI * 1000. * f64::from(i) / f64::from(SAMPLE_RATE);
            let sample = (16384. * phase.sin()).round() as i16;
            wav.extend(sample.to_le_bytes());
            wav.extend(sample.to_le_bytes());
        }
        std::fs::write(path, wav).expect("tone written");
    }

    fn real_recorder(target: String) -> Harness {
        Harness::new(Box::new(move |path| pw_record(&target, path)))
    }

    fn the_only_recording(dir: &Path) -> PathBuf {
        let mut wavs: Vec<_> = std::fs::read_dir(dir)
            .expect("recordings dir")
            .map(|entry| entry.expect("dir entry").path())
            .collect();
        assert_eq!(wavs.len(), 1, "{wavs:?}");
        wavs.remove(0)
    }

    #[test]
    fn records_the_tone_it_is_given() {
        let source = VirtualSource::new(true);
        let mut h = real_recorder(source.target());

        h.recorder.start();
        assert!(
            h.watch_until(Duration::from_secs(3), is_recording),
            "{}",
            h.notice()
        );
        std::thread::sleep(Duration::from_secs(1));
        h.recorder.stop();
        assert_eq!(h.state(), RecorderState::Idle);

        let wav = std::fs::read(the_only_recording(&h.dir)).expect("recording");
        let riff_len = u32::from_le_bytes(wav[4..8].try_into().expect("4 bytes"));
        assert_eq!(riff_len as usize, wav.len() - 8, "header finalised");

        let peak = peak_dbfs(&wav[WAV_HEADER_BYTES as usize..]).expect("not silent");
        assert!((peak + 6.02).abs() < 0.1, "peak {peak}");
    }

    /// WirePlumber's fallback would record the default source, the laptop mic.
    #[test]
    fn a_target_that_does_not_exist_records_nothing() {
        let mut h = real_recorder("turntable-test-no-such-node".into());

        h.recorder.start();
        assert!(h.watch_until(Duration::from_secs(3), |s| *s == RecorderState::Idle));
        assert_eq!(h.notice(), "Cannot record: target not found");
        assert!(file_size(&the_only_recording(&h.dir)) <= WAV_HEADER_BYTES);
    }

    /// The software version of pulling the interface's USB cable.
    #[test]
    fn a_source_vanishing_mid_set_ends_the_recording() {
        let source = VirtualSource::new(true);
        let mut h = real_recorder(source.target());

        h.recorder.start();
        assert!(
            h.watch_until(Duration::from_secs(3), is_recording),
            "{}",
            h.notice()
        );
        drop(source);

        assert!(h.watch_until(Duration::from_secs(3), |s| *s == RecorderState::Idle));
        let notice = h.notice();
        assert!(notice.starts_with("Recording stopped after "), "{notice}");
        assert!(notice.ends_with(": target not found"), "{notice}");
        assert!(file_size(&the_only_recording(&h.dir)) > WAV_HEADER_BYTES);
    }
}
