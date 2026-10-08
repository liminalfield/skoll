//! The background thread.
//!
//! The thread wakes at a fixed rate and reads the shared transport. In milestone 1 it only logs the
//! transport once per second. Later milestones give it the mpv process and socket.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::log;
use crate::transport::{format_time, SharedTransport, TransportSnapshot};

const WAKE_RATE_HZ: u32 = 60;
const LOG_INTERVAL: Duration = Duration::from_secs(1);

/// Owns the background thread. Dropping it stops and joins the thread.
pub struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn spawn(instance: u32, transport: Arc<SharedTransport>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = thread::Builder::new()
            .name(format!("skoll-worker-{instance}"))
            .spawn({
                let stop = stop.clone();
                move || run(instance, &transport, &stop)
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

fn run(instance: u32, transport: &SharedTransport, stop: &AtomicBool) {
    let period = Duration::from_secs(1) / WAKE_RATE_HZ;
    let mut next_wake = Instant::now();
    let mut next_log = Instant::now();
    let mut last_blocks = transport.load().blocks;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
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
    fn drop_stops_the_thread() {
        let worker = Worker::spawn(0, Arc::new(SharedTransport::default()));
        let start = Instant::now();
        drop(worker);
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
