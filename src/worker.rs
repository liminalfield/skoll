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
use crate::host_link::HostLink;
use crate::mpv::{self, Mpv};
use crate::sync::{Action, Sync};
use crate::transport::{format_time, SharedTransport, TransportSnapshot};
use crate::video_path::{PathAction, PathSync};
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
    let mut path_sync = PathSync::default();
    let mut synced_launch = 0;
    let mut was_playing = None;
    let mut process_watch = ProcessWatch::new(transport.load().blocks);
    let mut host_link = HostLink::new(instance);
    let period = Duration::from_secs(1) / WAKE_RATE_HZ;
    let mut next_wake = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let mut snapshot = transport.load();
        // Bitwig gives plugins a stale position while stopped. The extension, when running,
        // reports the real playhead. While playing, the plugin's own position is more precise.
        if let Some(host) = host_link.poll(now) {
            if !snapshot.playing {
                snapshot.pos_seconds = Some(host.playhead_seconds);
            }
        }
        if was_playing != Some(snapshot.playing) && snapshot.pos_seconds.is_some() {
            log!(instance, "transport {}", describe(&snapshot));
            was_playing = Some(snapshot.playing);
        }
        if let Some(message) = process_watch.update(now, snapshot.blocks) {
            log!(instance, "{message} ({})", describe(&snapshot));
        }

        supervisor.tick(now, params.show_video.value());
        if supervisor.launches != synced_launch {
            sync.reset_mpv();
            path_sync.reset_mpv();
            synced_launch = supervisor.launches;
        }
        if let Some(mpv) = supervisor.connected_mpv() {
            sync_video_path(instance, mpv, &mut path_sync, params);
            let offset = params.total_offset();
            for action in sync.tick(now, &snapshot, offset, mpv.state()) {
                apply(instance, mpv, &mut sync, &snapshot, offset, &action);
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

/// Keeps the stored video path and mpv's open file in step.
fn sync_video_path(instance: u32, mpv: &mut Mpv, path_sync: &mut PathSync, params: &SkollParams) {
    let action = {
        let Ok(stored) = params.video_path.path.read() else {
            return;
        };
        path_sync.tick(stored.as_deref(), mpv.path())
    };
    match action {
        Some(PathAction::Store(path)) => {
            log!(instance, "video: {path}");
            if let Ok(mut stored) = params.video_path.path.write() {
                *stored = Some(path);
            }
            params.video_path.dirty.store(true, Ordering::Relaxed);
        }
        Some(PathAction::Load(path)) => {
            if std::path::Path::new(&path).is_file() {
                log!(instance, "loading saved video: {path}");
                mpv.send(&json!({ "command": ["loadfile", path] }));
            } else {
                log!(instance, "saved video is missing, leaving mpv idle: {path}");
            }
        }
        None => {}
    }
}

/// How long a drift check waits for mpv's reply.
const TIME_POS_TIMEOUT: Duration = Duration::from_millis(50);

fn apply(
    instance: u32,
    mpv: &mut Mpv,
    sync: &mut Sync,
    snapshot: &TransportSnapshot,
    offset: f64,
    action: &Action,
) {
    match *action {
        Action::Seek { to, reason } => {
            log!(instance, "seek to {} ({reason})", format_time(to));
            mpv.send(&json!({ "command": ["seek", to, "absolute+exact"] }));
        }
        Action::SetPause(pause) => {
            log!(instance, "{} mpv", if pause { "pause" } else { "unpause" });
            mpv.send(&json!({ "command": ["set_property", "pause", pause] }));
        }
        Action::CheckDrift => {
            let Some(reading) = mpv.query_time_pos(TIME_POS_TIMEOUT) else {
                return;
            };
            let fps = mpv.state().fps;
            if let Some(check) = sync.on_time_pos(&reading, snapshot, offset, fps) {
                log!(
                    instance,
                    "drift {:+.1} ms ({:+} frames) at {}, query took {:.2} ms{}",
                    check.error_seconds * 1000.0,
                    check.error_frames,
                    format_time(check.expected),
                    reading.received.duration_since(reading.sent).as_secs_f64() * 1000.0,
                    if check.seek { ", correcting" } else { "" }
                );
            }
        }
    }
}

/// How long without a `process()` call counts as the host having stopped processing.
const PROCESS_STALL: Duration = Duration::from_millis(500);

/// Notices when the host stops and resumes calling `process()`. Bitwig may stop processing a
/// silent track while the transport is stopped (spec section 11), and the playhead is then
/// invisible to the plugin.
struct ProcessWatch {
    last_blocks: u64,
    last_change: Instant,
    stalled: bool,
}

impl ProcessWatch {
    fn new(blocks: u64) -> Self {
        Self {
            last_blocks: blocks,
            last_change: Instant::now(),
            stalled: false,
        }
    }

    /// Returns a message when processing stops or resumes.
    fn update(&mut self, now: Instant, blocks: u64) -> Option<String> {
        if blocks != self.last_blocks {
            let idle = now.duration_since(self.last_change);
            self.last_blocks = blocks;
            self.last_change = now;
            if self.stalled {
                self.stalled = false;
                return Some(format!(
                    "host resumed calling process() after {:.1} s",
                    idle.as_secs_f64()
                ));
            }
        } else if !self.stalled && now.duration_since(self.last_change) >= PROCESS_STALL {
            self.stalled = true;
            return Some("host stopped calling process()".to_owned());
        }
        None
    }
}

/// How long after the plugin is created before mpv first launches. The host restores the plugin
/// state just after creating it, and Show Video may be off: don't flash a window first.
const FIRST_LAUNCH_DELAY: Duration = Duration::from_millis(150);

/// Keeps one mpv process running while Show Video is on. Dropping it quits mpv and deletes the
/// socket.
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
    showing: bool,
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
            next_launch: Instant::now() + FIRST_LAUNCH_DELAY,
            relaunch_delay: MIN_RELAUNCH_DELAY,
            gave_up: false,
            launches: 0,
            showing: true,
        }
    }

    /// mpv, once its socket is connected.
    fn connected_mpv(&mut self) -> Option<&mut Mpv> {
        self.mpv.as_mut().filter(|mpv| mpv.is_connected())
    }

    fn tick(&mut self, now: Instant, show: bool) {
        if show != self.showing {
            self.showing = show;
            if show {
                log!(self.instance, "Show Video on");
                self.next_launch = self.next_launch.min(now);
                self.relaunch_delay = MIN_RELAUNCH_DELAY;
            } else {
                log!(self.instance, "Show Video off, closing mpv");
                // Dropping quits mpv and deletes the socket.
                self.mpv = None;
            }
        }
        if !show {
            return;
        }

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
        let prelude = config.script_prelude();
        match Mpv::launch(
            self.instance,
            config.mpv_path(),
            &args,
            &self.socket,
            &prelude,
        ) {
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
            pos_at: Instant::now(),
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
            pos_at: Instant::now(),
            sample_rate: 44_100.0,
            blocks: 0,
        };
        assert_eq!(describe(&snapshot), "stopped at an unknown position");
    }

    #[test]
    fn notices_process_stalls_once() {
        let t0 = Instant::now();
        let mut watch = ProcessWatch::new(0);
        watch.last_change = t0;
        assert_eq!(watch.update(t0 + Duration::from_millis(100), 5), None);
        assert_eq!(watch.update(t0 + Duration::from_millis(400), 5), None);
        assert_eq!(
            watch.update(t0 + Duration::from_millis(600), 5).as_deref(),
            Some("host stopped calling process()")
        );
        assert_eq!(watch.update(t0 + Duration::from_secs(2), 5), None);
        assert_eq!(
            watch.update(t0 + Duration::from_secs(3), 6).as_deref(),
            Some("host resumed calling process() after 2.9 s")
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
        let mut mpv = match Mpv::launch(0, Path::new("mpv"), &args, &socket, "") {
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
                        apply(0, &mut mpv, &mut sync, &transport, offset, &action);
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
            pos_at: Instant::now(),
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

        // Play from 20 s in real time. The frame on screen stays within one frame.
        let playing_from = |pos: f64| TransportSnapshot {
            playing: true,
            pos_at: Instant::now(),
            ..stopped_at(pos)
        };
        let frame_error = |transport: &TransportSnapshot| -> i64 {
            let before = Instant::now();
            let time_pos = query("time-pos").as_f64().unwrap();
            let expected = transport.pos_seconds_at(before).unwrap();
            (time_pos * 24.0).round() as i64 - (expected * 24.0).floor() as i64
        };
        let play = playing_from(20.0);
        run_for(play, 0.0, Duration::from_secs(2));
        assert_eq!(query("pause").as_bool(), Some(false));
        let error = frame_error(&play);
        assert!(error.abs() <= 1, "{error} frames off after 2 s");

        // A loop jumps back to 5 s: rule 4 follows it.
        let looped = playing_from(5.0);
        run_for(looped, 0.0, Duration::from_secs(1));
        let error = frame_error(&looped);
        assert!(error.abs() <= 1, "{error} frames off after the loop jump");
    }

    fn headless_mpv(_: u32) -> Config {
        Config {
            window_flags: Some(vec!["--vo=null".to_owned()]),
            ..Config::default()
        }
    }

    /// Ticks until `done`, or panics after 5 s.
    fn tick_until(supervisor: &mut Supervisor, show: bool, done: impl Fn(&mut Supervisor) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(supervisor) {
            assert!(Instant::now() < deadline, "timed out");
            supervisor.tick(Instant::now(), show);
            thread::sleep(Duration::from_millis(16));
        }
    }

    #[test]
    fn show_video_quits_and_relaunches_mpv() {
        if std::process::Command::new("mpv")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let mut supervisor = Supervisor::new(0, headless_mpv);
        tick_until(&mut supervisor, true, |s| s.connected_mpv().is_some());
        assert_eq!(supervisor.launches, 1);
        assert!(supervisor.socket.exists());

        supervisor.tick(Instant::now(), false);
        assert!(supervisor.mpv.is_none());
        assert!(!supervisor.socket.exists());
        // Stays closed while off.
        for _ in 0..5 {
            supervisor.tick(Instant::now(), false);
        }
        assert!(supervisor.mpv.is_none());

        // On again: relaunched straight away, not after the relaunch delay.
        supervisor.tick(Instant::now(), true);
        assert!(supervisor.mpv.is_some());
        assert_eq!(supervisor.launches, 2);
        tick_until(&mut supervisor, true, |s| s.connected_mpv().is_some());
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
