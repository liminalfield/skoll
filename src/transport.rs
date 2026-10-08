//! Transport state shared between `process()` and the background thread.
//!
//! `process()` writes these values and the background thread reads them. All fields are atomics
//! behind a sequence lock, so a reader always gets values from one block, and neither side ever
//! blocks or allocates.

use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The song position stored when the host reports none.
const NO_POSITION: f64 = f64::NAN;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// The reference point for timestamps stored as integers. Call once outside `process()` first,
/// so `process()` only ever reads it.
pub fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

pub struct SharedTransport {
    /// Odd while `store()` is writing. Each `store()` adds 2.
    seq: AtomicU64,
    playing: AtomicBool,
    /// The song position in seconds, stored as `f64` bits. NaN means unknown.
    pos_seconds: AtomicU64,
    /// When `process()` read the position, in nanoseconds since [`epoch()`].
    pos_at_nanos: AtomicU64,
    /// The sample rate in Hz, stored as `f32` bits.
    sample_rate: AtomicU32,
}

/// One consistent reading of the shared transport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransportSnapshot {
    pub playing: bool,
    pub pos_seconds: Option<f64>,
    /// When `process()` read `pos_seconds` from the host.
    pub pos_at: Instant,
    pub sample_rate: f32,
    /// The number of `process()` calls so far.
    pub blocks: u64,
}

impl TransportSnapshot {
    /// The song position at `t`, projected forward from when it was read while playing.
    pub fn pos_seconds_at(&self, t: Instant) -> Option<f64> {
        let pos = self.pos_seconds?;
        if !self.playing {
            return Some(pos);
        }
        let elapsed = match t.checked_duration_since(self.pos_at) {
            Some(d) => d.as_secs_f64(),
            None => -self.pos_at.duration_since(t).as_secs_f64(),
        };
        Some(pos + elapsed)
    }
}

impl Default for SharedTransport {
    fn default() -> Self {
        epoch();
        Self {
            seq: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            pos_seconds: AtomicU64::new(NO_POSITION.to_bits()),
            pos_at_nanos: AtomicU64::new(0),
            sample_rate: AtomicU32::new(0.0f32.to_bits()),
        }
    }
}

impl SharedTransport {
    /// Stores the transport. Called from `process()`, never from two threads at once.
    pub fn store(&self, playing: bool, pos_seconds: Option<f64>, at: Instant, sample_rate: f32) {
        let pos = pos_seconds.unwrap_or(NO_POSITION);
        let at_nanos = at.saturating_duration_since(epoch()).as_nanos() as u64;

        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.playing.store(playing, Ordering::Relaxed);
        self.pos_seconds.store(pos.to_bits(), Ordering::Relaxed);
        self.pos_at_nanos.store(at_nanos, Ordering::Relaxed);
        self.sample_rate
            .store(sample_rate.to_bits(), Ordering::Relaxed);
        self.seq.store(seq + 2, Ordering::Release);
    }

    /// Reads the transport. Called from the background thread.
    pub fn load(&self) -> TransportSnapshot {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let playing = self.playing.load(Ordering::Relaxed);
            let pos = f64::from_bits(self.pos_seconds.load(Ordering::Relaxed));
            let at_nanos = self.pos_at_nanos.load(Ordering::Relaxed);
            let sample_rate = f32::from_bits(self.sample_rate.load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return TransportSnapshot {
                    playing,
                    pos_seconds: (!pos.is_nan()).then_some(pos),
                    pos_at: epoch() + Duration::from_nanos(at_nanos),
                    sample_rate,
                    blocks: before / 2,
                };
            }
        }
    }
}

/// Formats seconds as `M:SS.mmm`, with a leading minus sign for negative times.
pub fn format_time(seconds: f64) -> String {
    let sign = if seconds < 0.0 { "-" } else { "" };
    let total_ms = (seconds.abs() * 1000.0).round() as u64;
    let minutes = total_ms / 60_000;
    let secs = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    format!("{sign}{minutes}:{secs:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_stopped_with_no_position() {
        let t = SharedTransport::default();
        assert_eq!(
            t.load(),
            TransportSnapshot {
                playing: false,
                pos_seconds: None,
                pos_at: epoch(),
                sample_rate: 0.0,
                blocks: 0,
            }
        );
    }

    #[test]
    fn round_trips_values() {
        let t = SharedTransport::default();
        let at = epoch() + Duration::from_millis(1500);
        t.store(true, Some(12.345), at, 48_000.0);
        assert_eq!(
            t.load(),
            TransportSnapshot {
                playing: true,
                pos_seconds: Some(12.345),
                pos_at: at,
                sample_rate: 48_000.0,
                blocks: 1,
            }
        );
        t.store(false, None, at, 44_100.0);
        assert_eq!(t.load().pos_seconds, None);
        assert_eq!(t.load().blocks, 2);
    }

    #[test]
    fn projects_position_while_playing() {
        let at = epoch() + Duration::from_secs(10);
        let mut snapshot = TransportSnapshot {
            playing: true,
            pos_seconds: Some(5.0),
            pos_at: at,
            sample_rate: 48_000.0,
            blocks: 1,
        };
        let later = at + Duration::from_millis(250);
        let earlier = at - Duration::from_millis(250);
        assert_eq!(snapshot.pos_seconds_at(later), Some(5.25));
        assert_eq!(snapshot.pos_seconds_at(earlier), Some(4.75));
        snapshot.playing = false;
        assert_eq!(snapshot.pos_seconds_at(later), Some(5.0));
    }

    #[test]
    fn formats_time() {
        assert_eq!(format_time(0.0), "0:00.000");
        assert_eq!(format_time(12.3456), "0:12.346");
        assert_eq!(format_time(61.5), "1:01.500");
        assert_eq!(format_time(3725.0), "62:05.000");
        assert_eq!(format_time(-1.25), "-0:01.250");
    }
}
