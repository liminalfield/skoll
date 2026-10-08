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

pub struct Mpv {
    instance: u32,
    child: Child,
    socket: PathBuf,
    stream: Option<UnixStream>,
    /// Bytes read from the socket that don't yet end in a newline.
    pending: Vec<u8>,
    launched: Instant,
    warned_no_socket: bool,
}

impl Mpv {
    /// Starts mpv. The socket appears a little later; [`Mpv::poll()`] connects to it.
    pub fn launch(
        instance: u32,
        program: &Path,
        args: &[String],
        socket: &Path,
    ) -> io::Result<Self> {
        remove_socket(socket);

        let mut command = Command::new(program);
        command
            .args(args)
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
        })
    }

    pub fn uptime(&self) -> Duration {
        self.launched.elapsed()
    }

    #[cfg(test)]
    fn is_connected(&self) -> bool {
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
        if let Some(event) = message.get("event") {
            log!(self.instance, "mpv event: {event}");
        } else if message.get("error").and_then(Value::as_str) != Some("success") {
            log!(self.instance, "mpv error reply: {message}");
        }
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
        remove_socket(&self.socket);
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

/// Deletes sockets left by host processes that died without cleaning up, for example after a crash.
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
            log!(instance, "deleting stale socket {}", entry.path().display());
            remove_socket(&entry.path());
        }
    }
}

/// The host pid in a socket name of the form `skoll-<pid>-<suffix>.sock`.
fn socket_owner_pid(name: &str) -> Option<libc::pid_t> {
    let rest = name.strip_prefix("skoll-")?.strip_suffix(".sock")?;
    let (pid, _suffix) = rest.split_once('-')?;
    pid.parse().ok().filter(|&pid| pid > 0)
}

fn remove_socket(socket: &Path) {
    match fs::remove_file(socket) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => log!(0, "could not delete {}: {err}", socket.display()),
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
    }

    #[test]
    fn parses_socket_owner_pid() {
        assert_eq!(socket_owner_pid("skoll-1234-0a1b2c3d.sock"), Some(1234));
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
