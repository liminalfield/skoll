//! One mpv process and its JSON IPC socket.

use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::log;

/// How long `quit` may take before the process is killed.
const QUIT_TIMEOUT: Duration = Duration::from_secs(1);
/// How long to wait for the socket before logging that it is missing.
const CONNECT_WARN_AFTER: Duration = Duration::from_secs(5);

/// The mpv script that opens a file dialog on right-click. Written next to the socket at launch.
const SCRIPT: &str = include_str!("skoll.lua");

/// IDs for `observe_property`. ID 1 is reserved for `path` (milestone 5).
const OBSERVE_PAUSE: u64 = 2;
const OBSERVE_EOF_REACHED: u64 = 3;
const OBSERVE_SEEKABLE: u64 = 4;
const OBSERVE_FPS: u64 = 5;
/// Request IDs for queries. Commands without an ID get replies with ID 0.
const FIRST_REQUEST_ID: u64 = 1000;

/// Events not worth logging: they arrive with every seek.
const QUIET_EVENTS: &[&str] = &[
    "seek",
    "playback-restart",
    "property-change",
    "video-reconfig",
    "audio-reconfig",
];

/// What Skoll knows about mpv, from its events and observed properties.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MpvState {
    /// A file is open and ready to seek. False while mpv is idle.
    pub file_loaded: bool,
    /// Counts loaded files, so a newly dropped file can be noticed.
    pub file_generation: u64,
    /// mpv's pause state. `None` until mpv reports it.
    pub paused: Option<bool>,
    /// mpv reached the end of the file. With `--keep-open` it then holds the last frame.
    pub eof_reached: bool,
    /// The file's frame rate (`container-fps`), when known.
    pub fps: Option<f64>,
}

/// One `time-pos` reading, with when the query was sent and the reply arrived.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimePosReading {
    pub time_pos: f64,
    pub sent: Instant,
    pub received: Instant,
}

pub struct Mpv {
    instance: u32,
    child: Child,
    socket: PathBuf,
    stream: Option<UnixStream>,
    /// Bytes read from the socket that don't yet end in a newline.
    pending: Vec<u8>,
    launched: Instant,
    warned_no_socket: bool,
    next_request_id: u64,
    /// The latest reply that carried a request ID.
    reply: Option<(u64, Value)>,
    state: MpvState,
}

impl Mpv {
    /// Starts mpv. The socket appears a little later; [`Mpv::poll()`] connects to it.
    pub fn launch(
        instance: u32,
        program: &Path,
        args: &[String],
        socket: &Path,
    ) -> io::Result<Self> {
        remove_file_quietly(socket);
        let script = script_path(socket);
        fs::write(&script, SCRIPT)?;
        let args: Vec<String> = args
            .iter()
            .cloned()
            .chain([format!("--script={}", script.display())])
            .collect();

        let mut command = Command::new(program);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: `prctl` is async-signal-safe and touches no memory of the parent.
        unsafe {
            command.pre_exec(|| {
                // If the thread that started mpv dies, for example because the host crashed, the
                // kernel sends mpv SIGTERM. No orphaned video windows.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;

        log!(
            instance,
            "launched mpv (pid {}): {} {}",
            child.id(),
            program.display(),
            args.join(" ")
        );
        Ok(Self {
            instance,
            child,
            socket: socket.to_path_buf(),
            stream: None,
            pending: Vec::new(),
            launched: Instant::now(),
            warned_no_socket: false,
            state: MpvState::default(),
            next_request_id: FIRST_REQUEST_ID,
            reply: None,
        })
    }

    pub fn state(&self) -> &MpvState {
        &self.state
    }

    pub fn uptime(&self) -> Duration {
        self.launched.elapsed()
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Connects to the socket if needed and reads pending messages. Returns the exit status once
    /// mpv has exited.
    pub fn poll(&mut self) -> Option<ExitStatus> {
        match self.child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(err) => log!(self.instance, "could not check mpv's status: {err}"),
        }

        if self.stream.is_none() {
            self.connect();
        }
        self.read_messages();
        None
    }

    /// Sends one command. Errors are logged and drop the connection; the next poll reconnects.
    pub fn send(&mut self, command: &Value) {
        let Some(stream) = self.stream.as_mut() else {
            log!(self.instance, "not connected to mpv, dropped {command}");
            return;
        };
        let mut line = command.to_string();
        line.push('\n');
        if let Err(err) = write_all_nonblocking(stream, line.as_bytes()) {
            log!(self.instance, "IPC error sending {command}: {err}");
            self.stream = None;
        }
    }

    /// Reads `time-pos`, waiting up to `timeout` for the reply. Blocks the calling thread, but
    /// mpv usually answers in well under a millisecond, and the send and receive times bracket
    /// the moment mpv read the value.
    pub fn query_time_pos(&mut self, timeout: Duration) -> Option<TimePosReading> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        let sent = Instant::now();
        self.send(&json!({ "command": ["get_property", "time-pos"], "request_id": id }));

        while sent.elapsed() < timeout {
            self.read_messages();
            if let Some((reply_id, reply)) = self.reply.take() {
                if reply_id == id {
                    let received = Instant::now();
                    let time_pos = reply.get("data").and_then(Value::as_f64)?;
                    return Some(TimePosReading {
                        time_pos,
                        sent,
                        received,
                    });
                }
            }
            // The connection dropped while waiting.
            self.stream.as_ref()?;
            thread::sleep(Duration::from_micros(100));
        }
        log!(
            self.instance,
            "no time-pos reply within {} ms",
            timeout.as_millis()
        );
        None
    }

    fn connect(&mut self) {
        match UnixStream::connect(&self.socket) {
            Ok(stream) => {
                if let Err(err) = stream.set_nonblocking(true) {
                    log!(self.instance, "IPC error: {err}");
                    return;
                }
                log!(
                    self.instance,
                    "connected to {} after {} ms",
                    self.socket.display(),
                    self.launched.elapsed().as_millis()
                );
                self.stream = Some(stream);
                self.pending.clear();
                // mpv replies to each with the current value, so the state is complete again
                // even after a reconnect.
                self.send(&json!({ "command": ["observe_property", OBSERVE_PAUSE, "pause"] }));
                self.send(&json!({
                    "command": ["observe_property", OBSERVE_EOF_REACHED, "eof-reached"]
                }));
                self.send(&json!({
                    "command": ["observe_property", OBSERVE_SEEKABLE, "seekable"]
                }));
                self.send(&json!({
                    "command": ["observe_property", OBSERVE_FPS, "container-fps"]
                }));
            }
            Err(err) => {
                if !self.warned_no_socket && self.launched.elapsed() > CONNECT_WARN_AFTER {
                    log!(
                        self.instance,
                        "still cannot connect to {}: {err}",
                        self.socket.display()
                    );
                    self.warned_no_socket = true;
                }
            }
        }
    }

    /// Reads everything mpv has sent. mpv sends events unprompted, so the socket must be drained
    /// even when no replies are expected.
    fn read_messages(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    log!(self.instance, "mpv closed the IPC connection");
                    self.stream = None;
                    return;
                }
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    log!(self.instance, "IPC error reading: {err}");
                    self.stream = None;
                    return;
                }
            }
        }

        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..end]);
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => self.handle_message(&message),
                Err(err) => log!(self.instance, "unreadable IPC message {line:?}: {err}"),
            }
        }
    }

    fn handle_message(&mut self, message: &Value) {
        let Some(event) = message.get("event").and_then(Value::as_str) else {
            if message.get("error").and_then(Value::as_str) != Some("success") {
                log!(self.instance, "mpv error reply: {message}");
            }
            match message.get("request_id").and_then(Value::as_u64) {
                Some(id) if id >= FIRST_REQUEST_ID => self.reply = Some((id, message.clone())),
                _ => {}
            }
            return;
        };
        if !QUIET_EVENTS.contains(&event) {
            log!(self.instance, "mpv event: {message}");
        }
        apply_event(&mut self.state, event, message);
    }

    /// Sends `quit`, waits briefly, kills mpv if needed and deletes the socket.
    fn shutdown(&mut self) {
        let status = match self.child.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => {
                if self.stream.is_none() {
                    self.connect();
                }
                if self.stream.is_some() {
                    self.send(&json!({ "command": ["quit"] }));
                }
                if self.stream.is_none() {
                    // mpv has not opened its socket yet, or the send failed. mpv also quits
                    // cleanly on SIGTERM.
                    // SAFETY: the child has not been reaped, so its pid is still ours.
                    unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
                }
                self.wait_for_exit(QUIT_TIMEOUT)
            }
        };

        match status {
            Some(status) => log!(self.instance, "mpv exited: {status}"),
            None => {
                log!(self.instance, "mpv did not quit, killing it");
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        remove_file_quietly(&self.socket);
        remove_file_quietly(&script_path(&self.socket));
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Deletes sockets and scripts left by host processes that died without cleaning up, for example
/// after a crash.
pub fn remove_stale_sockets(instance: u32, dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(socket_owner_pid) else {
            continue;
        };
        // SAFETY: signal 0 only checks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            log!(instance, "deleting stale file {}", entry.path().display());
            remove_file_quietly(&entry.path());
        }
    }
}

/// The mpv script's path: the socket's, with a `.lua` extension.
fn script_path(socket: &Path) -> PathBuf {
    socket.with_extension("lua")
}

/// The host pid in a file name of the form `skoll-<pid>-<suffix>.sock` or `.lua`.
fn socket_owner_pid(name: &str) -> Option<libc::pid_t> {
    let rest = name.strip_prefix("skoll-")?;
    let rest = rest
        .strip_suffix(".sock")
        .or_else(|| rest.strip_suffix(".lua"))?;
    let (pid, _suffix) = rest.split_once('-')?;
    pid.parse().ok().filter(|&pid| pid > 0)
}

/// Updates the state from one mpv event.
///
/// Whether a file is loaded comes from the `seekable` property, not from `file-loaded` events:
/// mpv sends an observed property's current value on connect, so a file that loaded before Skoll
/// connected is still noticed. `seekable` has no value while no file is open, including between
/// two files.
fn apply_event(state: &mut MpvState, event: &str, message: &Value) {
    if event != "property-change" {
        return;
    }
    // mpv sends no data while no file is open.
    let data = message.get("data").and_then(Value::as_bool);
    match message.get("id").and_then(Value::as_u64) {
        Some(OBSERVE_PAUSE) => state.paused = data,
        Some(OBSERVE_EOF_REACHED) => state.eof_reached = data.unwrap_or(false),
        Some(OBSERVE_FPS) => {
            state.fps = message
                .get("data")
                .and_then(Value::as_f64)
                .filter(|fps| *fps > 0.0)
        }
        Some(OBSERVE_SEEKABLE) => {
            let loaded = data.is_some();
            if loaded && !state.file_loaded {
                state.file_generation += 1;
            }
            state.file_loaded = loaded;
        }
        _ => {}
    }
}

fn remove_file_quietly(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => log!(0, "could not delete {}: {err}", path.display()),
    }
}

/// Writes a whole message to a non-blocking socket. Messages are small, so a full socket buffer
/// means mpv has stopped reading.
fn write_all_nonblocking(stream: &mut UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(100);
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(err) if err.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths;

    /// Launches a real mpv without a window. Skipped when mpv is not installed.
    fn launch_headless(socket: &Path) -> Option<Mpv> {
        let args: Vec<String> = ["--idle=yes", "--no-terminal", "--vo=null", "--no-audio"]
            .iter()
            .map(|&f| f.to_owned())
            .chain([format!("--input-ipc-server={}", socket.display())])
            .collect();
        match Mpv::launch(0, Path::new("mpv"), &args, socket) {
            Ok(mpv) => Some(mpv),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => panic!("could not launch mpv: {err}"),
        }
    }

    fn poll_until_connected(mpv: &mut Mpv) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !mpv.is_connected() {
            assert!(mpv.poll().is_none(), "mpv exited early");
            assert!(Instant::now() < deadline, "never connected");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn launch_connect_and_quit_removes_socket() {
        let socket = paths::socket_path();
        let Some(mut mpv) = launch_headless(&socket) else {
            return;
        };
        poll_until_connected(&mut mpv);
        assert!(socket.exists());

        let pid = mpv.child.id() as i32;
        drop(mpv);
        assert!(!socket.exists());
        assert!(!script_path(&socket).exists());
        // The process is gone and reaped.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[test]
    fn notices_when_mpv_exits() {
        let socket = paths::socket_path();
        let Some(mut mpv) = launch_headless(&socket) else {
            return;
        };
        poll_until_connected(&mut mpv);
        mpv.send(&json!({ "command": ["quit"] }));

        let deadline = Instant::now() + Duration::from_secs(5);
        while mpv.poll().is_none() {
            assert!(Instant::now() < deadline, "mpv did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        drop(mpv);
        assert!(!socket.exists());
        assert!(!script_path(&socket).exists());
    }

    fn event(text: &str) -> (String, Value) {
        let message: Value = serde_json::from_str(text).unwrap();
        (message["event"].as_str().unwrap().to_owned(), message)
    }

    #[test]
    fn tracks_state_from_events() {
        let mut state = MpvState::default();
        // Messages as mpv 0.41 sends them when a file is dropped onto the window.
        for text in [
            r#"{"event":"property-change","id":2,"name":"pause","data":true}"#,
            r#"{"event":"property-change","id":3,"name":"eof-reached"}"#,
            r#"{"event":"property-change","id":4,"name":"seekable"}"#,
            r#"{"event":"start-file","playlist_entry_id":1}"#,
            r#"{"event":"file-loaded"}"#,
            r#"{"event":"property-change","id":3,"name":"eof-reached","data":false}"#,
            r#"{"event":"property-change","id":4,"name":"seekable","data":true}"#,
        ] {
            let (name, message) = event(text);
            apply_event(&mut state, &name, &message);
        }
        assert_eq!(
            state,
            MpvState {
                file_loaded: true,
                file_generation: 1,
                paused: Some(true),
                eof_reached: false,
                fps: None,
            }
        );

        // The same file is dropped again.
        for text in [
            r#"{"event":"property-change","id":3,"name":"eof-reached","data":true}"#,
            r#"{"event":"end-file","reason":"stop","playlist_entry_id":1}"#,
            r#"{"event":"property-change","id":4,"name":"seekable"}"#,
            r#"{"event":"file-loaded"}"#,
            r#"{"event":"property-change","id":4,"name":"seekable","data":true}"#,
        ] {
            let (name, message) = event(text);
            apply_event(&mut state, &name, &message);
        }
        assert!(state.file_loaded);
        assert!(state.eof_reached);
        assert_eq!(state.file_generation, 2);
    }

    /// The right-click script runs the dialog and loads what it prints. A fake zenity stands in
    /// for the real dialog. Skipped without mpv or the test clip.
    #[test]
    fn dialog_script_loads_the_chosen_file() {
        use std::os::unix::fs::PermissionsExt;

        let clip = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-media/sync-test-24fps.mkv");
        if !clip.exists() {
            return;
        }
        let socket = paths::socket_path();
        let fake_zenity = socket.with_extension("zenity");
        fs::write(
            &fake_zenity,
            format!("#!/bin/sh\necho '{}'\n", clip.display()),
        )
        .unwrap();
        fs::set_permissions(&fake_zenity, fs::Permissions::from_mode(0o755)).unwrap();

        let args: Vec<String> = ["--idle=yes", "--no-terminal", "--vo=null", "--no-audio"]
            .iter()
            .map(|&f| f.to_owned())
            .chain([
                format!("--script-opts=skoll-zenity={}", fake_zenity.display()),
                format!("--input-ipc-server={}", socket.display()),
            ])
            .collect();
        let mut mpv = match Mpv::launch(0, Path::new("mpv"), &args, &socket) {
            Ok(mpv) => mpv,
            Err(err) if err.kind() == ErrorKind::NotFound => return,
            Err(err) => panic!("{err}"),
        };
        poll_until_connected(&mut mpv);
        assert!(script_path(&socket).exists());

        // Give mpv a moment to load the script, then "right-click".
        thread::sleep(Duration::from_millis(300));
        mpv.send(&json!({ "command": ["script-message", "skoll-open"] }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !mpv.state().file_loaded {
            assert!(mpv.poll().is_none(), "mpv exited");
            assert!(Instant::now() < deadline, "the file never loaded");
            thread::sleep(Duration::from_millis(10));
        }

        drop(mpv);
        fs::remove_file(&fake_zenity).unwrap();
        assert!(!script_path(&socket).exists());
    }

    #[test]
    fn parses_socket_owner_pid() {
        assert_eq!(socket_owner_pid("skoll-1234-0a1b2c3d.sock"), Some(1234));
        assert_eq!(socket_owner_pid("skoll-1234-0a1b2c3d.lua"), Some(1234));
        assert_eq!(socket_owner_pid("skoll-0-0a1b2c3d.sock"), None);
        assert_eq!(socket_owner_pid("skoll-x-0a1b2c3d.sock"), None);
        assert_eq!(socket_owner_pid("other-1234-0a1b2c3d.sock"), None);
        assert_eq!(socket_owner_pid("skoll-1234.sock"), None);
    }

    #[test]
    fn removes_only_stale_sockets() {
        let dir = std::env::temp_dir().join(format!("skoll-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // A pid above the kernel's maximum never exists.
        let stale = dir.join("skoll-2147483646-00000000.sock");
        let live = dir.join(format!("skoll-{}-00000000.sock", std::process::id()));
        let other = dir.join("unrelated.sock");
        for path in [&stale, &live, &other] {
            fs::write(path, "").unwrap();
        }

        remove_stale_sockets(0, &dir);
        assert!(!stale.exists());
        assert!(live.exists());
        assert!(other.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_binary_is_not_found() {
        let socket = paths::socket_path();
        let err = Mpv::launch(0, Path::new("/nonexistent/mpv"), &[], &socket)
            .err()
            .unwrap();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }
}
