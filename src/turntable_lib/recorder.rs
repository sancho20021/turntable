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
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
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
use crossbeam::channel::{Receiver, RecvTimeoutError, Sender};
use rustix::{
    io::Errno,
    process::{Pid, getppid, kill_process, set_parent_process_death_signal},
};

use crate::{
    decoder::SAMPLE_RATE,
    notices::Notices,
    pipewire_nodes::{NodeKind, recordable_nodes},
};

/// What libsndfile writes ahead of the first sample of a 16-bit stereo WAV.
const WAV_HEADER_BYTES: u64 = 44;

/// Past this, `pw-record` is wedged. SIGKILL then costs the header's length
/// field, not the audio.
const STOP_GRACE: Duration = Duration::from_secs(2);

const TICK: Duration = Duration::from_millis(100);

/// One second of the 16-bit stereo WAV, the stretch the level is read over.
const SECOND_BYTES: u64 = SAMPLE_RATE as u64 * 4;

/// `pw-record` writes every graph quantum, so a file this long without
/// growing has stopped receiving audio.
const STALL: Duration = Duration::from_secs(1);

const FREE_SPACE_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// exact PipeWire node name of the capture source
    pub source: String,
    /// existing folder the WAVs go into, never created
    pub dir: PathBuf,
}

pub enum RecorderCommand {
    /// Start recording, or stop the recording in progress.
    Toggle,
}

/// What the last second of a recording holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// its loudest sample, in dBFS
    Peak(f32),
    /// nothing but zeros
    Silent,
    /// nothing new: the file has not grown for [`STALL`]
    NotWriting,
}

/// Published for the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum RecorderState {
    Idle,
    /// `pw-record` is running and nothing has reached the file yet.
    Starting {
        path: PathBuf,
    },
    /// `since` is when the first audio reached the file. `free_bytes` is the
    /// space left in the recordings folder, `Some` once it has been read.
    Recording {
        path: PathBuf,
        since: Instant,
        signal: Signal,
        free_bytes: Option<u64>,
    },
    /// SIGINT sent, waiting for `pw-record` to finish the file.
    Stopping {
        path: PathBuf,
    },
    /// The last recording, finished.
    Saved {
        path: PathBuf,
        length: Duration,
    },
}

/// The running recorder: the channel commands go down, the state the TUI
/// draws, and the thread.
pub struct RecorderHandle {
    pub commands: Sender<RecorderCommand>,
    pub state: Arc<RwLock<RecorderState>>,
    shutdown: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl RecorderHandle {
    /// Stops a recording in progress, then waits for the thread to end.
    pub fn stop(self) -> std::thread::Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        self.thread.join()
    }
}

/// Warns about a source that is not there or a folder that does not exist,
/// then spawns the recorder thread.
///
/// It is also the thread that spawns `pw-record`, and PDEATHSIG ties the child
/// to it, so it must live until [`RecorderHandle::stop`]. See
/// [`spawn_with_deathsig`].
pub fn start(config: RecorderConfig, notices: Notices) -> RecorderHandle {
    log::info!(
        "Recording set up: source {}, folder {}",
        config.source,
        config.dir.display()
    );
    warn_about(&config, &notices);

    let (commands, receive) = crossbeam::channel::bounded(4);
    let state = Arc::new(RwLock::new(RecorderState::Idle));
    let shutdown = Arc::new(AtomicBool::new(false));
    let recorder = Recorder::new(
        config.dir,
        pw_record_for(config.source),
        Arc::clone(&state),
        notices,
        STOP_GRACE,
    );
    let thread = {
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || recorder.run(receive, shutdown))
    };

    RecorderHandle {
        commands,
        state,
        shutdown,
        thread,
    }
}

/// Only warns: the interface may be plugged in, or the drive mounted, after
/// launch.
fn warn_about(config: &RecorderConfig, notices: &Notices) {
    let problem = match recordable_nodes() {
        Err(e) => Some(format!("Cannot check the recording source: {e:#}")),
        Ok(nodes) if !nodes.iter().any(|node| node.name == config.source) => Some(format!(
            "Recording source {} not found; turntable list-devices shows the sources",
            config.source
        )),
        Ok(_) => None,
    };
    if let Some(problem) = problem {
        log::warn!("{problem}");
        notices.warn(problem);
    }

    if !config.dir.is_dir() {
        let problem = format!("Recordings folder {} does not exist", config.dir.display());
        log::warn!("{problem}");
        notices.warn(problem);
    }
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

fn record_args(node: &str, kind: NodeKind, path: &Path) -> Vec<OsString> {
    let monitor = match kind {
        NodeKind::Source => "",
        NodeKind::Sink => " stream.capture.sink = true",
    };
    vec![
        format!("--target={node}").into(),
        format!("--rate={SAMPLE_RATE}").into(),
        "--channels=2".into(),
        "--format=s16".into(),
        "-P".into(),
        format!(
            "{{ node.name = turntable-recorder node.dont-reconnect = true \
             node.dont-fallback = true{monitor} }}"
        )
        .into(),
        path.into(),
    ]
}

fn pw_record(node: &str, kind: NodeKind, path: &Path) -> Command {
    let mut command = Command::new("pw-record");
    command.args(record_args(node, kind, path));
    command
}

/// The `pw-record` command for `source`, looked up in [`recordable_nodes`] on
/// every start, so a sink is recorded through its monitor.
fn pw_record_for(source: String) -> CommandFor {
    Box::new(move |path| {
        let nodes = recordable_nodes().map_err(|e| format!("Cannot record: {e:#}"))?;
        let node = nodes
            .iter()
            .find(|node| node.name == source)
            .ok_or_else(|| format!("Cannot record: no source or output named {source}"))?;
        Ok(pw_record(&node.name, node.kind, path))
    })
}

fn output_path(dir: &Path, now: NaiveDateTime) -> PathBuf {
    dir.join(format!("mix_{}.wav", now.format("%Y-%m-%d_%H-%M-%S")))
}

/// `mm:ss`, or `h:mm:ss` from an hour up.
pub fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs / 3600 {
        0 => format!("{:02}:{:02}", secs / 60, secs % 60),
        hours => format!("{hours}:{:02}:{:02}", secs / 60 % 60, secs % 60),
    }
}

/// Deletes a WAV holding nothing but its header. Returns whether it did.
fn discard_empty(path: &Path) -> bool {
    file_size(path) <= WAV_HEADER_BYTES && std::fs::remove_file(path).is_ok()
}

/// The signal in the last second of a WAV that is `size` bytes long.
fn last_second(path: &Path, size: u64) -> Signal {
    let start = size.saturating_sub(SECOND_BYTES).max(WAV_HEADER_BYTES);
    let start = start + (start - WAV_HEADER_BYTES) % 2;

    let mut pcm = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(start))?;
        file.take(size - start).read_to_end(&mut pcm)
    });
    match read {
        Ok(_) => peak_dbfs(&pcm).map_or(Signal::Silent, Signal::Peak),
        Err(e) => {
            log::warn!("Cannot read {} for its level: {e}", path.display());
            Signal::NotWriting
        }
    }
}

fn free_bytes(dir: &Path) -> Option<u64> {
    rustix::fs::statvfs(dir)
        .ok()
        .map(|stats| stats.f_bavail * stats.f_frsize)
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
            set_parent_process_death_signal(Some(rustix::process::Signal::INT))?;
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
    if let Err(e) = kill_process(Pid::from_child(child), rustix::process::Signal::INT) {
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
/// fills and blocks `pw-record`. The thread ends when the child closes stderr.
fn drain_stderr(stderr: ChildStderr) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            log::warn!("pw-record: {line}");
        }
    })
}

/// A running `pw-record`: the child, the thread draining its stderr, and the
/// file it writes. Lives from spawn to reap, across Starting, Recording and
/// Stopping.
struct Capture {
    child: Child,
    stderr: JoinHandle<()>,
    path: PathBuf,
    size: u64,
    grew_at: Instant,
    free_bytes: Option<u64>,
    free_read_at: Option<Instant>,
}

/// Builds the recording command for a path, or says why there is none.
/// `pw-record` in production, a stand-in in the tests.
type CommandFor = Box<dyn Fn(&Path) -> Result<Command, String> + Send>;

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
                Ok(RecorderCommand::Toggle) if self.capture.is_some() => self.stop(),
                Ok(RecorderCommand::Toggle) => self.start(),
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

        if !self.dir.is_dir() {
            let message = format!(
                "Cannot record: recordings folder {} does not exist",
                self.dir.display()
            );
            log::error!("{message}");
            self.notices.error(message);
            return;
        }

        let path = output_path(&self.dir, chrono::Local::now().naive_local());
        let mut command = match (self.command_for)(&path) {
            Ok(command) => command,
            Err(message) => {
                log::error!("{message}");
                self.notices.error(message);
                return;
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = match spawn_with_deathsig(&mut command) {
            Ok(child) => child,
            Err(e) => {
                let message = format!("Cannot start pw-record: {e}");
                log::error!("{message}");
                self.notices.error(message);
                return;
            }
        };

        let stderr = drain_stderr(child.stderr.take().expect("stderr is piped"));
        log::info!("Starting pw-record into {}", path.display());
        self.publish(RecorderState::Starting { path: path.clone() });
        self.capture = Some(Capture {
            child,
            stderr,
            path,
            size: 0,
            grew_at: Instant::now(),
            free_bytes: None,
            free_read_at: None,
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
                let now = Instant::now();
                let size = file_size(&capture.path);
                if size != capture.size {
                    capture.size = size;
                    capture.grew_at = now;
                }
                if size <= WAV_HEADER_BYTES {
                    return;
                }

                if capture
                    .free_read_at
                    .is_none_or(|read_at| now - read_at >= FREE_SPACE_EVERY)
                {
                    capture.free_bytes = free_bytes(&self.dir);
                    capture.free_read_at = Some(now);
                }
                let signal = if now - capture.grew_at > STALL {
                    Signal::NotWriting
                } else {
                    last_second(&capture.path, size)
                };
                let (path, free_bytes) = (capture.path.clone(), capture.free_bytes);

                let since = match &self.state {
                    RecorderState::Recording { since, .. } => *since,
                    _ => {
                        log::info!("Recording to {}", path.display());
                        now
                    }
                };
                self.publish(RecorderState::Recording {
                    path,
                    since,
                    signal,
                    free_bytes,
                });
                return;
            }
            Ok(Some(status)) => status.to_string(),
            Err(e) => e.to_string(),
        };

        let capture = self.capture.take().expect("checked above");
        let _ = capture.stderr.join();

        let message = match &self.state {
            RecorderState::Recording { since, .. } => {
                log::error!(
                    "pw-record exited on its own ({exit}) while writing {}",
                    capture.path.display()
                );
                format!(
                    "Recording stopped after {}, see the log",
                    clock(since.elapsed())
                )
            }
            _ => {
                discard_empty(&capture.path);
                log::error!("pw-record quit before any audio ({exit}); nothing recorded");
                "Cannot record, see the log".to_string()
            }
        };
        self.notices.error(message);
        self.publish(RecorderState::Idle);
    }

    fn stop(&mut self) {
        let Some(mut capture) = self.capture.take() else {
            return;
        };

        let length = match &self.state {
            RecorderState::Recording { since, .. } => since.elapsed(),
            _ => Duration::ZERO,
        };
        self.publish(RecorderState::Stopping {
            path: capture.path.clone(),
        });

        let stopped = stop_child(&mut capture.child, self.grace);
        let _ = capture.stderr.join();

        let name = capture.path.display();
        if discard_empty(&capture.path) {
            log::info!("Stopped before any audio; nothing recorded");
            self.publish(RecorderState::Idle);
            return;
        }

        match stopped {
            Stopped::Clean => log::info!("Saved {name} ({})", clock(length)),
            Stopped::Killed => {
                log::error!("pw-record ignored SIGINT and was killed; {name} has no final header");
                self.notices.warn(format!(
                    "pw-record had to be killed, {name} may need repairing"
                ));
            }
        }
        self.publish(RecorderState::Saved {
            path: capture.path.clone(),
            length,
        });
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
            std::fs::create_dir_all(&dir).expect("scratch dir");
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

    /// The `-P` properties are what stop WirePlumber recording some other
    /// node, and what make an output record its monitor.
    #[test]
    fn the_properties_pin_the_node() {
        let props = |kind| {
            let args = record_args("node", kind, Path::new("/rec/mix.wav"));
            let at = args.iter().position(|arg| arg == "-P").expect("-P");
            args[at + 1].to_string_lossy().into_owned()
        };
        let (source, sink) = (props(NodeKind::Source), props(NodeKind::Sink));
        for props in [&source, &sink] {
            assert!(props.contains("node.dont-reconnect = true"), "{props}");
            assert!(props.contains("node.dont-fallback = true"), "{props}");
        }
        assert!(!source.contains("stream.capture.sink"), "{source}");
        assert!(sink.contains("stream.capture.sink = true"), "{sink}");
    }

    #[test]
    fn silence_has_no_peak_and_the_lowest_sample_does_not_overflow() {
        assert_eq!(peak_dbfs(&[0; 64]), None);
        assert_eq!(peak_dbfs(&[]), None);
        assert_eq!(peak_dbfs(&i16::MIN.to_le_bytes()), Some(0.));
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
        assert_eq!(
            status.and_then(|s| s.signal()),
            Some(rustix::process::Signal::INT.as_raw())
        );
    }

    #[test]
    fn stderr_is_drained() {
        // 200 KB is past a pipe's capacity: undrained, the child would block.
        let mut command = sh("yes x | head -c 200000 >&2");
        command.stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawns");
        let stderr = drain_stderr(child.stderr.take().expect("piped"));

        assert!(exited_within(&mut child, Duration::from_secs(5)).is_some());
        stderr.join().expect("drain thread");
    }

    #[test]
    fn a_missing_program_is_reported() {
        let mut h = Harness::new(Box::new(|_| Ok(Command::new("turntable-no-such-program"))));
        h.recorder.start();
        assert_eq!(h.state(), RecorderState::Idle);
        assert!(
            h.notice().starts_with("Cannot start pw-record"),
            "{}",
            h.notice()
        );
    }

    #[test]
    fn a_missing_folder_is_refused() {
        let mut h = Harness::new(Box::new(|_| Ok(sh("exec sleep 30"))));
        std::fs::remove_dir_all(&h.dir).expect("scratch dir");

        h.recorder.start();
        assert_eq!(h.state(), RecorderState::Idle);
        assert!(
            h.notice().starts_with("Cannot record: recordings folder "),
            "{}",
            h.notice()
        );
        assert!(!h.dir.exists(), "the folder was created");
    }

    #[test]
    fn stopping_before_any_audio_leaves_no_file() {
        let mut h = Harness::new(Box::new(|path| {
            let mut command = sh("head -c 44 /dev/zero > \"$1\"; exec sleep 30");
            command.arg("sh").arg(path);
            Ok(command)
        }));
        h.recorder.start();
        assert!(eventually(Duration::from_secs(2), || {
            std::fs::read_dir(&h.dir).expect("dir").count() == 1
        }));

        h.recorder.stop();
        assert_eq!(std::fs::read_dir(&h.dir).expect("dir").count(), 0);
        assert_eq!(h.state(), RecorderState::Idle);
    }

    #[test]
    fn silence_reads_as_silent() {
        let mut h = Harness::new(Box::new(|path| {
            let mut command = sh("head -c 44 /dev/zero > \"$1\"
                while :; do head -c 19200 /dev/zero >> \"$1\"; sleep 0.1; done");
            command.arg("sh").arg(path);
            Ok(command)
        }));
        h.recorder.start();
        assert!(h.watch_until(Duration::from_secs(2), |s| matches!(
            s,
            RecorderState::Recording {
                signal: Signal::Silent,
                ..
            }
        )));
    }

    #[test]
    fn a_file_that_stops_growing_reads_as_not_writing() {
        let mut h = Harness::new(Box::new(|path| {
            let mut command = sh("head -c 44 /dev/zero > \"$1\"
                head -c 4000 /dev/urandom >> \"$1\"; exec sleep 30");
            command.arg("sh").arg(path);
            Ok(command)
        }));
        h.recorder.start();
        assert!(h.watch_until(Duration::from_secs(2), is_recording));
        assert!(h.watch_until(Duration::from_secs(3), |s| matches!(
            s,
            RecorderState::Recording {
                signal: Signal::NotWriting,
                ..
            }
        )));
    }

    /// The recorder's own thread, with a stand-in that writes audio and waits.
    struct Running {
        commands: crossbeam::channel::Sender<RecorderCommand>,
        state: Arc<RwLock<RecorderState>>,
        shutdown: Arc<AtomicBool>,
        thread: JoinHandle<()>,
        dir: PathBuf,
    }

    impl Running {
        fn new() -> Self {
            let (commands, receive) = crossbeam::channel::bounded(4);
            let state = Arc::new(RwLock::new(RecorderState::Idle));
            let shutdown = Arc::new(AtomicBool::new(false));
            let dir = scratch_dir();
            std::fs::create_dir_all(&dir).expect("scratch dir");

            let recorder = Recorder::new(
                dir.clone(),
                Box::new(|path| {
                    let mut command = sh("head -c 1000 /dev/zero > \"$1\"; exec sleep 30");
                    command.arg("sh").arg(path);
                    Ok(command)
                }),
                Arc::clone(&state),
                Notices::new(),
                Duration::from_millis(200),
            );
            let thread = {
                let shutdown = Arc::clone(&shutdown);
                std::thread::spawn(move || recorder.run(receive, shutdown))
            };
            Self {
                commands,
                state,
                shutdown,
                thread,
                dir,
            }
        }

        fn toggle(&self) {
            self.commands
                .send(RecorderCommand::Toggle)
                .expect("recorder alive");
        }

        fn reaches(&self, done: impl Fn(&RecorderState) -> bool) -> bool {
            eventually(Duration::from_secs(2), || {
                done(&self.state.read().expect("lock"))
            })
        }

        fn shut_down(self) -> RecorderState {
            self.shutdown.store(true, Ordering::Relaxed);
            self.thread.join().expect("recorder thread");
            let _ = std::fs::remove_dir_all(&self.dir);
            self.state.read().expect("lock").clone()
        }
    }

    #[test]
    fn a_second_toggle_stops_the_recording() {
        let running = Running::new();
        running.toggle();
        assert!(running.reaches(is_recording));
        running.toggle();
        assert!(running.reaches(|s| matches!(s, RecorderState::Saved { .. })));
        running.shut_down();
    }

    #[test]
    fn shutdown_stops_the_recording() {
        let running = Running::new();
        running.toggle();
        assert!(running.reaches(is_recording));
        assert!(matches!(running.shut_down(), RecorderState::Saved { .. }));
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

        /// The sink side, which the tone plays into.
        fn sink(&self) -> String {
            format!("{}-in", self.name)
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

    fn real_recorder(source: String) -> Harness {
        Harness::new(pw_record_for(source))
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
        assert!(
            h.watch_until(Duration::from_secs(3), |s| matches!(
                s,
                RecorderState::Recording {
                    signal: Signal::Peak(peak),
                    free_bytes: Some(_),
                    ..
                } if (peak + 6.02).abs() < 0.1
            )),
            "{:?}",
            h.state()
        );
        std::thread::sleep(Duration::from_millis(500));
        h.recorder.stop();
        assert!(matches!(h.state(), RecorderState::Saved { .. }));

        let wav = std::fs::read(the_only_recording(&h.dir)).expect("recording");
        let riff_len = u32::from_le_bytes(wav[4..8].try_into().expect("4 bytes"));
        assert_eq!(riff_len as usize, wav.len() - 8, "header finalised");

        let peak = peak_dbfs(&wav[WAV_HEADER_BYTES as usize..]).expect("not silent");
        assert!((peak + 6.02).abs() < 0.1, "peak {peak}");
    }

    #[test]
    fn an_unknown_name_is_refused_before_pw_record_starts() {
        let mut h = real_recorder("turntable-test-no-such-node".into());

        h.recorder.start();
        assert_eq!(h.state(), RecorderState::Idle);
        assert_eq!(
            h.notice(),
            "Cannot record: no source or output named turntable-test-no-such-node"
        );
        assert_eq!(
            std::fs::read_dir(&h.dir).expect("recordings dir").count(),
            0
        );
    }

    /// A node can vanish between the lookup and the spawn. WirePlumber's
    /// fallback would then record the default source or the default sink's
    /// monitor: the laptop mic, or whatever is playing.
    #[test]
    fn pw_record_never_falls_back_to_another_node() {
        for kind in [NodeKind::Source, NodeKind::Sink] {
            let mut h = Harness::new(Box::new(move |path| {
                Ok(pw_record("turntable-test-no-such-node", kind, path))
            }));

            h.recorder.start();
            assert!(h.watch_until(Duration::from_secs(3), |s| *s == RecorderState::Idle));
            assert_eq!(
                std::fs::read_dir(&h.dir).expect("recordings dir").count(),
                0,
                "{kind:?}: nothing is left behind"
            );
            assert_eq!(h.notice(), "Cannot record, see the log");
        }
    }

    #[test]
    fn records_what_an_output_is_playing() {
        let source = VirtualSource::new(true);
        let mut h = real_recorder(source.sink());

        h.recorder.start();
        assert!(
            h.watch_until(Duration::from_secs(3), is_recording),
            "{}",
            h.notice()
        );
        std::thread::sleep(Duration::from_secs(1));
        h.recorder.stop();

        let wav = std::fs::read(the_only_recording(&h.dir)).expect("recording");
        let peak = peak_dbfs(&wav[WAV_HEADER_BYTES as usize..]).expect("not silent");
        assert!((peak + 6.02).abs() < 0.1, "peak {peak}");
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
        assert!(file_size(&the_only_recording(&h.dir)) > WAV_HEADER_BYTES);
        let notice = h.notice();
        assert!(notice.starts_with("Recording stopped after "), "{notice}");
    }
}
