//! The link to the Skoll Transport controller extension (`extension/`).
//!
//! Bitwig does not update the song position it gives plugins while the transport is stopped. The
//! extension reads the playhead through Bitwig's controller API and sends it here over UDP. See
//! `SkollExtension.java` for the protocol.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::log;
use crate::transport::format_time;

/// The extension's UDP port. Must match `SkollExtension.PORT`.
const EXTENSION_PORT: u16 = 58730;
const HELLO_INTERVAL: Duration = Duration::from_secs(1);
/// The extension answers every hello, so silence this long means it is gone.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(3);

/// Bitwig's transport as the extension reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostTransport {
    pub playing: bool,
    pub playhead_seconds: f64,
    pub play_start_seconds: f64,
}

pub struct HostLink {
    instance: u32,
    socket: Option<UdpSocket>,
    hello: Vec<u8>,
    next_hello: Instant,
    latest: Option<HostTransport>,
    last_message: Option<Instant>,
}

impl HostLink {
    pub fn new(instance: u32) -> Self {
        let (socket, hello) = match bind() {
            Ok((socket, port)) => (Some(socket), format!("hello {port}").into_bytes()),
            Err(err) => {
                log!(instance, "no link to the Bitwig extension: {err}");
                (None, Vec::new())
            }
        };
        Self {
            instance,
            socket,
            hello,
            next_hello: Instant::now(),
            latest: None,
            last_message: None,
        }
    }

    /// Says hello when due and reads what the extension sent. Returns Bitwig's transport while
    /// the extension is reachable.
    pub fn poll(&mut self, now: Instant) -> Option<HostTransport> {
        let socket = self.socket.as_ref()?;

        if now >= self.next_hello {
            let extension = SocketAddr::from((Ipv4Addr::LOCALHOST, EXTENSION_PORT));
            // Fails with ConnectionRefused while the extension isn't running. That's normal.
            let _ = socket.send_to(&self.hello, extension);
            self.next_hello = now + HELLO_INTERVAL;
        }

        let mut buf = [0u8; 256];
        loop {
            match socket.recv(&mut buf) {
                Ok(n) => match parse(&buf[..n]) {
                    Some(transport) => {
                        if self.last_message.is_none() {
                            log!(
                                self.instance,
                                "connected to the Bitwig extension: playhead {}, play start {}",
                                format_time(transport.playhead_seconds),
                                format_time(transport.play_start_seconds)
                            );
                        }
                        self.latest = Some(transport);
                        self.last_message = Some(now);
                    }
                    None => log!(
                        self.instance,
                        "unreadable message from the Bitwig extension: {:?}",
                        String::from_utf8_lossy(&buf[..n])
                    ),
                },
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                // An earlier hello found no listener.
                Err(err) if err.kind() == ErrorKind::ConnectionRefused => continue,
                Err(err) => {
                    log!(self.instance, "Bitwig extension link error: {err}");
                    break;
                }
            }
        }

        if let Some(last) = self.last_message {
            if now.duration_since(last) > SILENCE_TIMEOUT {
                log!(self.instance, "lost the Bitwig extension");
                self.last_message = None;
                self.latest = None;
            }
        }
        self.latest
    }
}

/// Binds a non-blocking UDP socket on a free localhost port.
fn bind() -> std::io::Result<(UdpSocket, u16)> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    socket.set_nonblocking(true)?;
    let port = socket.local_addr()?.port();
    Ok((socket, port))
}

/// Parses `skoll1 <playing 0|1> <playhead seconds> <play-start seconds>`.
fn parse(message: &[u8]) -> Option<HostTransport> {
    let text = std::str::from_utf8(message).ok()?;
    let mut fields = text.split_ascii_whitespace();
    if fields.next()? != "skoll1" {
        return None;
    }
    let playing = match fields.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let playhead_seconds = fields
        .next()?
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite())?;
    let play_start_seconds = fields
        .next()?
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite())?;
    fields.next().is_none().then_some(HostTransport {
        playing,
        playhead_seconds,
        play_start_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_messages() {
        assert_eq!(
            parse(b"skoll1 0 12.500000 4.000000"),
            Some(HostTransport {
                playing: false,
                playhead_seconds: 12.5,
                play_start_seconds: 4.0,
            })
        );
        assert_eq!(
            parse(b"skoll1 1 0.000000 0.000000").map(|t| t.playing),
            Some(true)
        );
        assert_eq!(parse(b"skoll1 2 0.0 0.0"), None);
        assert_eq!(parse(b"skoll2 0 0.0 0.0"), None);
        assert_eq!(parse(b"skoll1 0 NaN 0.0"), None);
        assert_eq!(parse(b"skoll1 0 1.0"), None);
        assert_eq!(parse(b"skoll1 0 1.0 2.0 3.0"), None);
    }

    /// A fake extension on a random port stands in for Bitwig.
    #[test]
    fn says_hello_and_reads_replies() {
        let fake_extension = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        fake_extension
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let mut link = HostLink::new(0);
        let hello = String::from_utf8(link.hello.clone()).unwrap();
        let port: u16 = hello.strip_prefix("hello ").unwrap().parse().unwrap();
        let plugin = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

        let t0 = Instant::now();
        assert_eq!(link.poll(t0), None);
        fake_extension
            .send_to(b"skoll1 0 7.250000 0.000000", plugin)
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(link.poll(t0).map(|t| t.playhead_seconds), Some(7.25));
        // Silence: the link gives up on the extension.
        assert_eq!(link.poll(t0 + SILENCE_TIMEOUT * 2), None);
    }
}
