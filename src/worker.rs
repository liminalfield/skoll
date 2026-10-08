//! The background thread.
//!
//! The thread wakes at a fixed rate. It owns the mpv process and its socket, relaunches mpv when it
//! exits, and applies the sync rules.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::config::Config;
use crate::mpv::{self, Mpv};
use crate::sync::{Action, Sync};
use crate::transport::{format_time, SharedTransport, TransportSnapshot};
use crate::{log, paths, SkollParams};

const WAKE_RATE_HZ: u32 = 60;

/// The relaunch delay after mpv exits. It doubles while mpv keeps exiting soon after launch.
const MIN_RELAUNCH_DELAY: Duration = Duration::from_secs(1);
const MAX_RELAUNCH_DELAY: Duration = Duration::from_secs(30);
/// mpv running at least this long counts as a normal exit, such as the user closing the window.
const STABLE_UPTIME: Duration = Duration::from_secs(10);

/// Reads the config. A parameter so tests can avoid opening an mpv window.
pub type ConfigLoader = fn(u32) -> Config;

/// Owns the background thread. Dropping it stops and joins the thread.
pub struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn spawn(
        instance: u32,
        transport: Arc<SharedTransport>,
        params: Arc<SkollParams>,
        load_config: ConfigLoader,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new()
            .name(format!("skoll-worker-{instance}"))
            .spawn({
                let stop = stop.clone();
                move || run(instance, &transport, &params, &stop, load_config)
            });

        let handle = match handle {
            Ok(handle) => Some(handle),
            Err(err) => {
                log!(instance, "could not start the background thread: {err}");
                None
            }
        };
        Self { stop, handle }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(
    instance: u32,
    transport: &SharedTransport,
    params: &SkollParams,
    stop: &AtomicBool,
    load_config: ConfigLoader,
) {
    let mut supervisor = Supervisor::new(instance, load_config);
    let mut sync = Sync::default();
    let mut synced_launch = 0;
    let mut was_playing = None;
    let period = Duration::from_secs(1) / WAKE_RATE_HZ;
    let mut next_wake = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let snapshot = transport.load();
        if was_playing != Some(snapshot.playing) && snapshot.pos_seconds.is_some() {
            log!(instance, "transport {}", describe(&snapshot));
            was_playing = Some(snapshot.playing);
        }

        supervisor.tick(now);
        if supervisor.launches != synced_launch {
            sync.reset_mpv();
            synced_launch = supervisor.launches;
        }
        if let Some(mpv) = supervisor.connected_mpv() {
            let offset = f64::from(params.offset.value());
            for action in sync.tick(now, &snapshot, offset, mpv.state()) {
                apply(instance, mpv, &action);
            }
        }

        next_wake += period;
        let now = Instant::now();
        if next_wake > now {
            thread::sleep(next_wake - now);
        } else {
            // We fell behind, for example after a system suspend. Don't try to catch up.
            next_wake = now;
        }
    }
}

fn apply(instance: u32, mpv: &mut Mpv, action: &Action) {
    match *action {
        Action::Seek { to, reason } => {
            log!(instance, "seek to {} ({reason})", format_time(to));
            mpv.send(&json!({ "command": ["seek", to, "absolute+exact"] }));
        }
        Action::SetPause(pause) => {
            log!(instance, "{} mpv", if pause { "pause" } else { "unpause" });
            mpv.send(&json!({ "command": ["set_property", "pause", pause] }));
        }
    }
}

/// Keeps one mpv process running. Dropping it quits mpv and deletes the socket.
struct Supervisor {
    instance: u32,
    load_config: ConfigLoader,
    socket: PathBuf,
    mpv: Option<Mpv>,
    next_launch: Instant,
    relaunch_delay: Duration,
    /// Set when mpv is not installed. Audio still passes through.
    gave_up: bool,
    /// Counts successful launches, so the sync state can be reset for a new mpv.
    launches: u64,
}

impl Supervisor {
    fn new(instance: u32, load_config: ConfigLoader) -> Self {
        let socket = paths::socket_path();
        if let Some(dir) = socket.parent() {
            mpv::remove_stale_sockets(instance, dir);
        }
        Self {
            instance,
            load_config,
            socket,
            mpv: None,
            next_launch: Instant::now(),
            relaunch_delay: MIN_RELAUNCH_DELAY,
            gave_up: false,
            launches: 0,
        }
    }

    /// mpv, once its socket is connected.
    fn connected_mpv(&mut self) -> Option<&mut Mpv> {
        self.mpv.as_mut().filter(|mpv| mpv.is_connected())
    }

    fn tick(&mut self, now: Instant) {
        if let Some(mpv) = &mut self.mpv {
            if mpv.poll().is_some() {
                let uptime = mpv.uptime();
                // Dropping logs the exit status and deletes the socket.
                self.mpv = None;
                self.relaunch_delay = next_relaunch_delay(self.relaunch_delay, uptime);
                self.next_launch = now + self.relaunch_delay;
                log!(
                    self.instance,
                    "mpv ran for {:.1} s, relaunching in {} s",
                    uptime.as_secs_f64(),
                    self.relaunch_delay.as_secs()
                );
            }
            return;
        }

        if self.gave_up || now < self.next_launch {
            return;
        }
        let config = (self.load_config)(self.instance);
        let args = config.mpv_args(&self.socket);
        match Mpv::launch(self.instance, config.mpv_path(), &args, &self.socket) {
            Ok(mpv) => {
                self.mpv = Some(mpv);
                self.launches += 1;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                log!(
                    self.instance,
                    "mpv not found ({}: {err}). Install mpv or set mpv_path in the config file. \
                     Audio still passes through.",
                    config.mpv_path().display()
                );
                self.gave_up = true;
            }
            Err(err) => {
                self.relaunch_delay = next_relaunch_delay(self.relaunch_delay, Duration::ZERO);
                self.next_launch = now + self.relaunch_delay;
                log!(
                    self.instance,
                    "could not launch mpv: {err}. Retrying in {} s",
                    self.relaunch_delay.as_secs()
                );
            }
        }
    }
}

fn next_relaunch_delay(previous: Duration, uptime: Duration) -> Duration {
    if uptime >= STABLE_UPTIME {
        MIN_RELAUNCH_DELAY
    } else {
        (previous * 2).min(MAX_RELAUNCH_DELAY)
    }
}

/// The transport for a log line, such as `playing at 1:01.500 (61.500000 s)`.
fn describe(snapshot: &TransportSnapshot) -> String {
    let state = if snapshot.playing {
        "playing"
    } else {
        "stopped"
    };
    match snapshot.pos_seconds {
        Some(seconds) => format!("{state} at {} ({seconds:.6} s)", format_time(seconds)),
        None => format!("{state} at an unknown position"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_playing_transport() {
        let snapshot = TransportSnapshot {
            playing: true,
            pos_seconds: Some(61.5),
            sample_rate: 48_000.0,
            blocks: 0,
        };
        assert_eq!(describe(&snapshot), "playing at 1:01.500 (61.500000 s)");
    }

    #[test]
    fn describes_unknown_position() {
        let snapshot = TransportSnapshot {
            playing: false,
            pos_seconds: None,
            sample_rate: 44_100.0,
            blocks: 0,
        };
        assert_eq!(describe(&snapshot), "stopped at an unknown position");
    }

    #[test]
    fn relaunch_delay_backs_off_while_mpv_keeps_crashing() {
        let crash = Duration::from_millis(200);
        let mut delay = MIN_RELAUNCH_DELAY;
        let mut delays = Vec::new();
        for _ in 0..7 {
            delay = next_relaunch_delay(delay, crash);
            delays.push(delay.as_secs());
        }
        assert_eq!(delays, [2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(
            next_relaunch_delay(delay, STABLE_UPTIME),
            MIN_RELAUNCH_DELAY
        );
    }

    /// Drives a real, windowless mpv through the sync rules with the test clip, and reads the
    /// resulting position over a second IPC connection. Skipped without mpv or the clip.
    #[test]
    fn stopped_follow_with_real_mpv() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::path::Path;

        let clip = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-media/sync-test-24fps.mkv");
        if !clip.exists() {
            return;
        }
        let socket = paths::socket_path();
        let mut args: Vec<String> = Config {
            window_flags: Some(vec!["--vo=null".to_owned()]),
            ..Config::default()
        }
        .mpv_args(&socket);
        args.push(clip.display().to_string());
        let mut mpv = match Mpv::launch(0, Path::new("mpv"), &args, &socket) {
            Ok(mpv) => mpv,
            Err(err) if err.kind() == ErrorKind::NotFound => return,
            Err(err) => panic!("{err}"),
        };

        let mut sync = Sync::default();
        let mut run_for = |transport: TransportSnapshot, offset: f64, duration: Duration| {
            let end = Instant::now() + duration;
            while Instant::now() < end {
                assert!(mpv.poll().is_none(), "mpv exited");
                if mpv.is_connected() {
                    for action in sync.tick(Instant::now(), &transport, offset, mpv.state()) {
                        apply(0, &mut mpv, &action);
                    }
                }
                thread::sleep(Duration::from_millis(16));
            }
            *mpv.state()
        };
        let query = |property: &str| -> serde_json::Value {
            let mut stream = UnixStream::connect(&socket).unwrap();
            let request = json!({ "command": ["get_property", property], "request_id": 7 });
            writeln!(stream, "{request}").unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
                if reply["request_id"] == 7 {
                    return reply["data"].clone();
                }
            }
        };
        let stopped_at = |pos: f64| TransportSnapshot {
            playing: false,
            pos_seconds: Some(pos),
            sample_rate: 48_000.0,
            blocks: 0,
        };

        let state = run_for(stopped_at(61.5), 0.0, Duration::from_secs(2));
        assert!(state.file_loaded);
        assert_eq!(query("time-pos").as_f64(), Some(61.5));
        assert_eq!(query("pause").as_bool(), Some(true));

        // Scrub backwards, then before the video starts.
        run_for(stopped_at(12.25), 0.0, Duration::from_millis(300));
        assert_eq!(query("time-pos").as_f64(), Some(12.25));
        run_for(stopped_at(3.0), 10.0, Duration::from_millis(300));
        assert_eq!(query("time-pos").as_f64(), Some(0.0));

        // Someone unpauses mpv by hand: rule 5 pauses it again.
        let mut stream = UnixStream::connect(&socket).unwrap();
        writeln!(
            stream,
            "{}",
            json!({ "command": ["set_property", "pause", false] })
        )
        .unwrap();
        run_for(stopped_at(3.0), 10.0, Duration::from_millis(300));
        assert_eq!(query("pause").as_bool(), Some(true));
    }

    fn missing_mpv(_: u32) -> Config {
        Config {
            mpv_path: Some("/nonexistent/mpv".into()),
            ..Config::default()
        }
    }

    #[test]
    fn drop_stops_the_thread() {
        let worker = Worker::spawn(
            0,
            Arc::new(SharedTransport::default()),
            Arc::new(SkollParams::default()),
            missing_mpv,
        );
        let start = Instant::now();
        drop(worker);
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
