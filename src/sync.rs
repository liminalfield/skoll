//! The sync rules from spec section 7, as a pure state machine.
//!
//! The background thread calls [`Sync::tick()`] on each wake and sends the returned actions to
//! mpv. Milestone 3 implements rules 1, 5 and 6.

use std::time::{Duration, Instant};

use crate::mpv::MpvState;
use crate::transport::TransportSnapshot;

/// Rule 6: at most 30 seeks per second. The background thread wakes at 60 Hz, so this allows a
/// seek on every second wake, with slack for wake-up jitter.
const MIN_SEEK_INTERVAL: Duration = Duration::from_millis(32);
/// How long to wait for mpv to confirm a pause change before sending it again.
const PAUSE_RESEND_AFTER: Duration = Duration::from_millis(500);
/// Song positions closer than this count as unchanged.
const POSITION_EPSILON: f64 = 1e-9;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Seek to this video time in seconds. Never negative: mpv counts negative absolute seeks
    /// back from the end of the file.
    Seek {
        to: f64,
        reason: &'static str,
    },
    SetPause(bool),
}

#[derive(Debug, Default)]
pub struct Sync {
    last_target: Option<f64>,
    was_playing: bool,
    /// The latest seek target not yet sent. Rule 6: newer targets replace it.
    pending_seek: Option<(f64, &'static str)>,
    last_seek: Option<Instant>,
    seen_file_generation: u64,
    pause_sent: Option<(bool, Instant)>,
}

impl Sync {
    /// Forgets everything known about mpv. Call after mpv relaunches.
    pub fn reset_mpv(&mut self) {
        self.seen_file_generation = 0;
        self.pause_sent = None;
        self.last_seek = None;
    }

    pub fn tick(
        &mut self,
        now: Instant,
        transport: &TransportSnapshot,
        offset: f64,
        mpv: &MpvState,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(pos) = transport.pos_seconds else {
            return actions;
        };
        let target = pos - offset;
        let playing = transport.playing;

        // Rule 1: while stopped, follow the playhead.
        if !playing {
            if self.was_playing {
                self.pending_seek = Some((target, "transport stopped"));
            } else if self
                .last_target
                .is_none_or(|last| (last - target).abs() > POSITION_EPSILON)
            {
                self.pending_seek = Some((target, "playhead moved"));
            }
        }
        if mpv.file_generation != self.seen_file_generation {
            self.seen_file_generation = mpv.file_generation;
            if !playing {
                self.pending_seek = Some((target, "file loaded"));
            }
        }
        self.last_target = Some(target);
        self.was_playing = playing;

        // Rule 5: mpv's pause state follows the transport. At the end of the file mpv pauses
        // itself and ignores unpausing, so don't fight it there.
        let want_pause = !playing;
        if let Some(paused) = mpv.paused {
            let at_end = paused && mpv.eof_reached && !want_pause;
            let recently_sent = self.pause_sent.is_some_and(|(sent, at)| {
                sent == want_pause && now.duration_since(at) < PAUSE_RESEND_AFTER
            });
            if paused != want_pause && !at_end && !recently_sent {
                actions.push(Action::SetPause(want_pause));
                self.pause_sent = Some((want_pause, now));
            }
        }

        // Rule 6: throttle seeks; only the latest target is sent.
        let seek_allowed = self
            .last_seek
            .is_none_or(|last| now.duration_since(last) >= MIN_SEEK_INTERVAL);
        if mpv.file_loaded && seek_allowed {
            if let Some((to, reason)) = self.pending_seek.take() {
                actions.push(Action::Seek {
                    to: to.max(0.0),
                    reason,
                });
                self.last_seek = Some(now);
            }
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped_at(pos: f64) -> TransportSnapshot {
        TransportSnapshot {
            playing: false,
            pos_seconds: Some(pos),
            sample_rate: 48_000.0,
            blocks: 0,
        }
    }

    fn playing_at(pos: f64) -> TransportSnapshot {
        TransportSnapshot {
            playing: true,
            ..stopped_at(pos)
        }
    }

    fn loaded(paused: bool) -> MpvState {
        MpvState {
            file_loaded: true,
            file_generation: 1,
            paused: Some(paused),
            eof_reached: false,
        }
    }

    fn seeks(actions: &[Action]) -> Vec<f64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Seek { to, .. } => Some(*to),
                Action::SetPause(_) => None,
            })
            .collect()
    }

    /// Ticks at 60 Hz starting from `start`, returning the time of the next tick.
    fn frame(start: Instant, n: u32) -> Instant {
        start + Duration::from_secs(1) / 60 * n
    }

    #[test]
    fn seeks_when_a_file_loads_then_only_on_change() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(true);

        assert_eq!(
            seeks(&sync.tick(frame(t0, 0), &stopped_at(5.0), 0.0, &mpv)),
            [5.0]
        );
        assert!(sync
            .tick(frame(t0, 1), &stopped_at(5.0), 0.0, &mpv)
            .is_empty());
        assert!(sync
            .tick(frame(t0, 2), &stopped_at(5.0), 0.0, &mpv)
            .is_empty());
        assert_eq!(
            seeks(&sync.tick(frame(t0, 3), &stopped_at(7.25), 0.0, &mpv)),
            [7.25]
        );
    }

    #[test]
    fn applies_offset_and_clamps_negative_times() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(true);

        assert_eq!(
            seeks(&sync.tick(frame(t0, 0), &stopped_at(12.0), 10.0, &mpv)),
            [2.0]
        );
        assert_eq!(
            seeks(&sync.tick(frame(t0, 3), &stopped_at(4.0), 10.0, &mpv)),
            [0.0]
        );
        // Changing the offset alone moves the picture too.
        assert_eq!(
            seeks(&sync.tick(frame(t0, 6), &stopped_at(4.0), 1.0, &mpv)),
            [3.0]
        );
    }

    #[test]
    fn throttles_to_30_seeks_per_second_keeping_the_latest() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(true);

        // Scrub at 60 Hz for one second: a new position on every tick.
        let mut sent = Vec::new();
        for n in 0..60 {
            let pos = f64::from(n) * 0.1;
            sent.extend(seeks(&sync.tick(frame(t0, n), &stopped_at(pos), 0.0, &mpv)));
        }
        assert_eq!(sent.len(), 30);
        // Scrubbing stops; the last position still arrives.
        sent.extend(seeks(&sync.tick(
            frame(t0, 60),
            &stopped_at(5.9),
            0.0,
            &mpv,
        )));
        sent.extend(seeks(&sync.tick(
            frame(t0, 61),
            &stopped_at(5.9),
            0.0,
            &mpv,
        )));
        assert_eq!(sent.last(), Some(&5.9));
    }

    #[test]
    fn waits_for_a_file_before_seeking() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let idle = MpvState {
            paused: Some(true),
            ..MpvState::default()
        };

        assert!(sync
            .tick(frame(t0, 0), &stopped_at(5.0), 0.0, &idle)
            .is_empty());
        assert!(sync
            .tick(frame(t0, 1), &stopped_at(8.0), 0.0, &idle)
            .is_empty());
        // The file loads: seek to where the playhead is now.
        assert_eq!(
            seeks(&sync.tick(frame(t0, 2), &stopped_at(8.0), 0.0, &loaded(true))),
            [8.0]
        );
    }

    #[test]
    fn seeks_to_the_stop_position() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(false);

        sync.tick(frame(t0, 0), &playing_at(1.0), 0.0, &mpv);
        sync.tick(frame(t0, 1), &playing_at(1.016), 0.0, &mpv);
        let actions = sync.tick(frame(t0, 2), &stopped_at(1.016), 0.0, &mpv);
        assert_eq!(seeks(&actions), [1.016]);
        assert!(actions.contains(&Action::SetPause(true)));
    }

    #[test]
    fn does_not_seek_while_playing() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(false);
        for n in 0..10 {
            let pos = f64::from(n) / 60.0;
            assert!(seeks(&sync.tick(frame(t0, n), &playing_at(pos), 0.0, &mpv)).is_empty());
        }
    }

    #[test]
    fn corrects_pause_state_without_spamming() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        sync.tick(frame(t0, 0), &stopped_at(1.0), 0.0, &loaded(true));

        // The user unpauses mpv by hand: the next tick pauses it again.
        let unpaused = loaded(false);
        let actions = sync.tick(frame(t0, 1), &stopped_at(1.0), 0.0, &unpaused);
        assert_eq!(actions, [Action::SetPause(true)]);
        // mpv hasn't confirmed yet: don't resend every tick.
        assert!(sync
            .tick(frame(t0, 2), &stopped_at(1.0), 0.0, &unpaused)
            .is_empty());
        // Still unconfirmed after half a second: resend.
        let later = frame(t0, 1) + PAUSE_RESEND_AFTER;
        assert_eq!(
            sync.tick(later, &stopped_at(1.0), 0.0, &unpaused),
            [Action::SetPause(true)]
        );
    }

    #[test]
    fn unpauses_when_playing_except_at_the_end_of_the_file() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        assert_eq!(
            sync.tick(frame(t0, 0), &playing_at(1.0), 0.0, &loaded(true)),
            [Action::SetPause(false)]
        );

        let mut sync = Sync::default();
        let at_end = MpvState {
            eof_reached: true,
            ..loaded(true)
        };
        assert!(sync
            .tick(frame(t0, 0), &playing_at(400.0), 0.0, &at_end)
            .is_empty());
    }

    #[test]
    fn unknown_position_does_nothing() {
        let mut sync = Sync::default();
        let transport = TransportSnapshot {
            pos_seconds: None,
            ..stopped_at(0.0)
        };
        assert!(sync
            .tick(Instant::now(), &transport, 0.0, &loaded(false))
            .is_empty());
    }
}
