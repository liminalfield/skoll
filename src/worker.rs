//! The background thread.
//!
//! The thread wakes at a fixed rate. It owns the mpv process and its socket, relaunches mpv when it
//! exits, and logs the transport once per second.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::mpv::{self, Mpv};
use crate::transport::{format_time, SharedTransport, TransportSnapshot};
use crate::{log, paths};

const WAKE_RATE_HZ: u32 = 60;
const LOG_INTERVAL: Duration = Duration::from_secs(1);

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
        load_config: ConfigLoader,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new()
            .name(format!("skoll-worker-{instance}"))
            .spawn({
                let stop = stop.clone();
                move || run(instance, &transport, &stop, load_config)
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

fn run(instance: u32, transport: &SharedTransport, stop: &AtomicBool, load_config: ConfigLoader) {
    let mut supervisor = Supervisor::new(instance, load_config);
    let period = Duration::from_secs(1) / WAKE_RATE_HZ;
    let mut next_wake = Instant::now();
    let mut next_log = Instant::now();
    let mut last_blocks = transport.load().blocks;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        supervisor.tick(now);

        if now >= next_log {
            let snapshot = transport.load();
            log!(
                instance,
                "{}",
                describe(&snapshot, snapshot.blocks - last_blocks)
            );
            last_blocks = snapshot.blocks;
            next_log += LOG_INTERVAL;
        }

        next_wake += period;
        let now = Instant::now();
        if next_wake > now {
            thread::sleep(next_wake - now);
        } else {
            // We fell behind, for example after a system suspend. Don't try to catch up.
            next_wake = now;
            next_log = next_log.max(now);
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
        }
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
            Ok(mpv) => self.mpv = Some(mpv),
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

/// One log line for the transport. `blocks` is the number of `process()` calls since the last
/// line, so zero means the host has stopped processing.
fn describe(snapshot: &TransportSnapshot, blocks: u64) -> String {
    let state = if snapshot.playing {
        "playing"
    } else {
        "stopped"
    };
    let pos = match snapshot.pos_seconds {
        Some(seconds) => format!("{} ({seconds:.6} s)", format_time(seconds)),
        None => "unknown".to_owned(),
    };
    format!(
        "{state} pos={pos} sr={} blocks={blocks}",
        snapshot.sample_rate
    )
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
        assert_eq!(
            describe(&snapshot, 94),
            "playing pos=1:01.500 (61.500000 s) sr=48000 blocks=94"
        );
    }

    #[test]
    fn describes_unknown_position() {
        let snapshot = TransportSnapshot {
            playing: false,
            pos_seconds: None,
            sample_rate: 44_100.0,
            blocks: 0,
        };
        assert_eq!(
            describe(&snapshot, 0),
            "stopped pos=unknown sr=44100 blocks=0"
        );
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

    fn missing_mpv(_: u32) -> Config {
        Config {
            mpv_path: Some("/nonexistent/mpv".into()),
            ..Config::default()
        }
    }

    #[test]
    fn drop_stops_the_thread() {
        let worker = Worker::spawn(0, Arc::new(SharedTransport::default()), missing_mpv);
        let start = Instant::now();
        drop(worker);
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
