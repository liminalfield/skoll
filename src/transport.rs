//! Transport state shared between `process()` and the background thread.
//!
//! `process()` writes these values and the background thread reads them. All fields are atomics,
//! so neither side ever blocks or allocates.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// The song position stored when the host reports none.
const NO_POSITION: f64 = f64::NAN;

pub struct SharedTransport {
    playing: AtomicBool,
    /// The song position in seconds, stored as `f64` bits. NaN means unknown.
    pos_seconds: AtomicU64,
    /// The sample rate in Hz, stored as `f32` bits.
    sample_rate: AtomicU32,
    /// The number of `store()` calls, which is the number of `process()` calls.
    blocks: AtomicU64,
}

/// One consistent-enough reading of the shared transport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransportSnapshot {
    pub playing: bool,
    pub pos_seconds: Option<f64>,
    pub sample_rate: f32,
    pub blocks: u64,
}

impl Default for SharedTransport {
    fn default() -> Self {
        Self {
            playing: AtomicBool::new(false),
            pos_seconds: AtomicU64::new(NO_POSITION.to_bits()),
            sample_rate: AtomicU32::new(0.0f32.to_bits()),
            blocks: AtomicU64::new(0),
        }
    }
}

impl SharedTransport {
    /// Stores the transport. Called from `process()`.
    pub fn store(&self, playing: bool, pos_seconds: Option<f64>, sample_rate: f32) {
        let pos = pos_seconds.unwrap_or(NO_POSITION);
        self.playing.store(playing, Ordering::Relaxed);
        self.pos_seconds.store(pos.to_bits(), Ordering::Relaxed);
        self.sample_rate
            .store(sample_rate.to_bits(), Ordering::Relaxed);
        self.blocks.fetch_add(1, Ordering::Relaxed);
    }

    /// Reads the transport. Called from the background thread.
    pub fn load(&self) -> TransportSnapshot {
        let pos = f64::from_bits(self.pos_seconds.load(Ordering::Relaxed));
        TransportSnapshot {
            playing: self.playing.load(Ordering::Relaxed),
            pos_seconds: (!pos.is_nan()).then_some(pos),
            sample_rate: f32::from_bits(self.sample_rate.load(Ordering::Relaxed)),
            blocks: self.blocks.load(Ordering::Relaxed),
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
                sample_rate: 0.0,
                blocks: 0,
            }
        );
    }

    #[test]
    fn round_trips_values() {
        let t = SharedTransport::default();
        t.store(true, Some(12.345), 48_000.0);
        assert_eq!(
            t.load(),
            TransportSnapshot {
                playing: true,
                pos_seconds: Some(12.345),
                sample_rate: 48_000.0,
                blocks: 1,
            }
        );
        t.store(false, None, 44_100.0);
        assert_eq!(t.load().pos_seconds, None);
        assert_eq!(t.load().blocks, 2);
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
