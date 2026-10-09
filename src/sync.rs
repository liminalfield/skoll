//! The sync rules from spec section 7, as a pure state machine.
//!
//! The background thread calls [`Sync::tick()`] on each wake and carries out the returned
//! actions. For [`Action::CheckDrift`] it reads mpv's `time-pos` and passes the reading to
//! [`Sync::on_time_pos()`].

use std::time::{Duration, Instant};

use crate::mpv::{MpvState, TimePosReading};
use crate::transport::TransportSnapshot;

/// Rule 6: at most 30 seeks per second. The background thread wakes at 60 Hz, so this allows a
/// seek on every second wake, with slack for wake-up jitter.
const MIN_SEEK_INTERVAL: Duration = Duration::from_millis(32);
/// How long to wait for mpv to confirm a pause change before sending it again.
const PAUSE_RESEND_AFTER: Duration = Duration::from_millis(500);
/// Song positions closer than this count as unchanged.
const POSITION_EPSILON: f64 = 1e-9;
/// Rule 3: check drift twice per second.
const DRIFT_CHECK_INTERVAL: Duration = Duration::from_millis(500);
/// No drift check this soon after a seek, while mpv restarts playback.
const SETTLE_AFTER_SEEK: Duration = Duration::from_millis(300);
/// Rule 3: seek when the frame on screen is more than this many frames off.
const MAX_FRAME_ERROR: i64 = 1;
/// Rule 4: a position change this much bigger or smaller than the elapsed time is a jump.
const JUMP_TOLERANCE: f64 = 0.050;
/// The frame rate when mpv reports none.
const DEFAULT_FPS: f64 = 24.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Seek to this video time in seconds. Never negative: mpv counts negative absolute seeks
    /// back from the end of the file.
    Seek {
        to: f64,
        reason: &'static str,
    },
    SetPause(bool),
    /// Read mpv's `time-pos` and pass it to [`Sync::on_time_pos()`].
    CheckDrift,
}

/// The result of one drift check, for the log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftCheck {
    /// The video time that should be on screen.
    pub expected: f64,
    /// `time-pos` minus `expected`. mpv's `time-pos` is the shown frame's timestamp, so a video
    /// in sync reads up to one frame behind.
    pub error_seconds: f64,
    /// The shown frame minus the frame that should be shown.
    pub error_frames: i64,
    /// A seek was requested to correct the drift.
    pub seek: bool,
}

#[derive(Debug, Default)]
pub struct Sync {
    last_target: Option<f64>,
    /// The last song position and when it was read, while playing. For rule 4.
    last_playing_pos: Option<(f64, Instant)>,
    was_playing: bool,
    /// The reason for a seek not yet sent. The target is computed when it is sent, so rule 6's
    /// "only the latest" holds.
    pending_seek: Option<&'static str>,
    last_seek: Option<Instant>,
    /// Where the last seek went, to skip seeks that would show the same frame.
    last_seek_to: Option<f64>,
    seen_file_generation: u64,
    pause_sent: Option<(bool, Instant)>,
    next_drift_check: Option<Instant>,
}

impl Sync {
    /// Forgets everything known about mpv. Call after mpv relaunches.
    pub fn reset_mpv(&mut self) {
        self.seen_file_generation = 0;
        self.pause_sent = None;
        self.last_seek = None;
        self.last_seek_to = None;
    }

    pub fn tick(
        &mut self,
        now: Instant,
        transport: &TransportSnapshot,
        offset: f64,
        mpv: &MpvState,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let (Some(pos), Some(pos_now)) = (transport.pos_seconds, transport.pos_seconds_at(now))
        else {
            return actions;
        };
        let target = pos_now - offset;
        let playing = transport.playing;

        if playing {
            if !self.was_playing {
                // Rule 2.
                self.pending_seek = Some("transport started");
            } else if let Some((last_pos, last_at)) = self.last_playing_pos {
                // Rule 4. Compare the host's positions with the time between them.
                let elapsed = transport.pos_at.saturating_duration_since(last_at);
                let expected = last_pos + elapsed.as_secs_f64();
                if (pos - expected).abs() > JUMP_TOLERANCE {
                    self.pending_seek = Some("position jumped");
                }
            }
            // While the video time is negative mpv holds frame 0. Start it when it reaches 0.
            if target >= 0.0 && self.last_target.is_some_and(|last| last < 0.0) {
                self.pending_seek = Some("video starts");
            }
            self.last_playing_pos = Some((pos, transport.pos_at));
        } else {
            // Rule 1.
            if self.was_playing {
                self.pending_seek = Some("transport stopped");
            } else if self
                .last_target
                .is_none_or(|last| (last - target).abs() > POSITION_EPSILON)
            {
                self.pending_seek = Some("playhead moved");
            }
            self.last_playing_pos = None;
        }
        if mpv.file_generation != self.seen_file_generation {
            self.seen_file_generation = mpv.file_generation;
            self.pending_seek = Some("file loaded");
        }
        self.last_target = Some(target);
        self.was_playing = playing;

        // Rule 6: throttle seeks.
        let seek_allowed = self
            .last_seek
            .is_none_or(|last| now.duration_since(last) >= MIN_SEEK_INTERVAL);
        if mpv.file_loaded && seek_allowed {
            if let Some(reason) = self.pending_seek.take() {
                let to = target.max(0.0);
                // Moving the playhead or Offset within one frame, or while the video time stays
                // below 0, shows the same frame.
                let fps = mpv.fps.unwrap_or(DEFAULT_FPS);
                let same_frame = reason == "playhead moved"
                    && self
                        .last_seek_to
                        .is_some_and(|last| frame_index(last, fps) == frame_index(to, fps));
                if !same_frame {
                    actions.push(Action::Seek { to, reason });
                    self.last_seek = Some(now);
                    self.last_seek_to = Some(to);
                }
            }
        }

        // Rule 5: mpv's pause state follows the transport. Rule 2: unpause only after the seek
        // has gone out. At the end of the file mpv pauses itself and ignores unpausing, so don't
        // fight it there.
        let want_pause = !playing || target < 0.0;
        if let Some(paused) = mpv.paused {
            let at_end = paused && mpv.eof_reached && !want_pause;
            let seek_first = !want_pause && self.pending_seek.is_some();
            let recently_sent = self.pause_sent.is_some_and(|(sent, at)| {
                sent == want_pause && now.duration_since(at) < PAUSE_RESEND_AFTER
            });
            if paused != want_pause && !at_end && !seek_first && !recently_sent {
                actions.push(Action::SetPause(want_pause));
                self.pause_sent = Some((want_pause, now));
            }
        }

        // Rule 3: check drift while the video plays.
        let settled = self
            .last_seek
            .is_none_or(|last| now.duration_since(last) >= SETTLE_AFTER_SEEK);
        let check_due = self.next_drift_check.is_none_or(|next| now >= next);
        if playing
            && target >= 0.0
            && mpv.file_loaded
            && mpv.paused == Some(false)
            && !mpv.eof_reached
            && self.pending_seek.is_none()
            && settled
            && check_due
        {
            actions.push(Action::CheckDrift);
            self.next_drift_check = Some(now + DRIFT_CHECK_INTERVAL);
        }

        actions
    }

    /// Rule 3: compares a `time-pos` reading with the expected video time, and requests a seek
    /// when the shown frame is more than one frame off.
    pub fn on_time_pos(
        &mut self,
        reading: &TimePosReading,
        transport: &TransportSnapshot,
        offset: f64,
        fps: Option<f64>,
    ) -> Option<DriftCheck> {
        // mpv read the value between sending and receiving; take the midpoint.
        let read_at = reading.sent + reading.received.duration_since(reading.sent) / 2;
        let expected = transport.pos_seconds_at(read_at)? - offset;
        let fps = fps.unwrap_or(DEFAULT_FPS);

        // `time-pos` is the shown frame's timestamp: round to its frame. The expected time falls
        // somewhere inside the frame that should show: floor to that frame.
        let shown_frame = (reading.time_pos * fps).round() as i64;
        let expected_frame = (expected * fps).floor() as i64;
        let error_frames = shown_frame - expected_frame;
        let seek = error_frames.abs() > MAX_FRAME_ERROR;
        if seek && self.pending_seek.is_none() {
            self.pending_seek = Some("drift");
        }
        Some(DriftCheck {
            expected,
            error_seconds: reading.time_pos - expected,
            error_frames,
            seek,
        })
    }
}

/// The frame showing at video time `t`.
fn frame_index(t: f64, fps: f64) -> i64 {
    // The epsilon keeps a time exactly on a frame boundary in that frame despite rounding.
    (t * fps + 1e-6).floor() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped_at(pos: f64) -> TransportSnapshot {
        TransportSnapshot {
            playing: false,
            pos_seconds: Some(pos),
            pos_at: Instant::now(),
            sample_rate: 48_000.0,
            blocks: 0,
        }
    }

    /// Playing, with `pos` read at `at`.
    fn playing_at(pos: f64, at: Instant) -> TransportSnapshot {
        TransportSnapshot {
            playing: true,
            pos_at: at,
            ..stopped_at(pos)
        }
    }

    fn loaded(paused: bool) -> MpvState {
        MpvState {
            file_loaded: true,
            file_generation: 1,
            paused: Some(paused),
            eof_reached: false,
            fps: Some(24.0),
            window_id: None,
        }
    }

    fn seeks(actions: &[Action]) -> Vec<f64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Seek { to, .. } => Some(*to),
                _ => None,
            })
            .collect()
    }

    fn seek_reasons(actions: &[Action]) -> Vec<&'static str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Seek { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    }

    /// The `n`th 60 Hz tick after `start`.
    fn frame(start: Instant, n: u32) -> Instant {
        start + Duration::from_secs(1) / 60 * n
    }

    fn assert_close(actual: &[f64], expected: &[f64]) {
        assert_eq!(actual.len(), expected.len(), "{actual:?} != {expected:?}");
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-6, "{actual:?} != {expected:?}");
        }
    }

    // Rules 1, 5 and 6: stopped.

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
    fn skips_seeks_that_show_the_same_frame() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(true);

        // Offset 10 s with the playhead at 1 s: frame 0. Turning Offset further changes nothing.
        assert_eq!(
            seeks(&sync.tick(frame(t0, 0), &stopped_at(1.0), 10.0, &mpv)),
            [0.0]
        );
        assert!(seeks(&sync.tick(frame(t0, 3), &stopped_at(1.0), 11.0, &mpv)).is_empty());
        assert!(seeks(&sync.tick(frame(t0, 6), &stopped_at(1.0), 12.0, &mpv)).is_empty());
        // Within one 24 fps frame (41.7 ms): no seek. Into the next frame: seek.
        sync.tick(frame(t0, 9), &stopped_at(5.0), 0.0, &mpv);
        assert!(seeks(&sync.tick(frame(t0, 12), &stopped_at(5.03), 0.0, &mpv)).is_empty());
        assert_eq!(
            seeks(&sync.tick(frame(t0, 15), &stopped_at(5.05), 0.0, &mpv)),
            [5.05]
        );
        // A stop always seeks, to correct wherever playback left mpv.
        sync.tick(frame(t0, 18), &playing_at(5.05, frame(t0, 18)), 0.0, &mpv);
        let actions = sync.tick(frame(t0, 21), &stopped_at(5.05), 0.0, &mpv);
        assert_eq!(seek_reasons(&actions), ["transport stopped"]);
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

        sync.tick(frame(t0, 0), &playing_at(1.0, frame(t0, 0)), 0.0, &mpv);
        sync.tick(frame(t0, 1), &playing_at(1.016, frame(t0, 1)), 0.0, &mpv);
        let actions = sync.tick(frame(t0, 4), &stopped_at(1.016), 0.0, &mpv);
        assert_eq!(seeks(&actions), [1.016]);
        assert!(actions.contains(&Action::SetPause(true)));
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

    // Rule 2: transport starts.

    #[test]
    fn seeks_then_unpauses_when_the_transport_starts() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        sync.tick(frame(t0, 0), &stopped_at(3.0), 0.0, &loaded(true));

        // The position was read 5 ms before this tick: seek to where it is now.
        let read_at = frame(t0, 3) - Duration::from_millis(5);
        let actions = sync.tick(frame(t0, 3), &playing_at(3.0, read_at), 0.0, &loaded(true));
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert_close(&seeks(&actions), &[3.005]);
        assert_eq!(actions[1], Action::SetPause(false));
    }

    #[test]
    fn holds_unpause_until_the_throttled_seek_goes_out() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        sync.tick(frame(t0, 0), &stopped_at(3.0), 0.0, &loaded(true));
        // A scrub seek just went out.
        sync.tick(frame(t0, 2), &stopped_at(4.0), 0.0, &loaded(true));

        // Play on the next tick: the throttle holds the seek, so mpv stays paused.
        let actions = sync.tick(
            frame(t0, 3),
            &playing_at(4.0, frame(t0, 3)),
            0.0,
            &loaded(true),
        );
        assert!(actions.is_empty(), "{actions:?}");
        let actions = sync.tick(
            frame(t0, 4),
            &playing_at(4.0, frame(t0, 3)),
            0.0,
            &loaded(true),
        );
        assert_eq!(seek_reasons(&actions), ["transport started"]);
        assert_eq!(actions.last(), Some(&Action::SetPause(false)));
    }

    #[test]
    fn stays_paused_at_the_end_of_the_file() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let at_end = MpvState {
            eof_reached: true,
            ..loaded(true)
        };
        sync.tick(frame(t0, 0), &playing_at(400.0, t0), 0.0, &at_end);
        let actions = sync.tick(
            frame(t0, 3),
            &playing_at(400.05, frame(t0, 3)),
            0.0,
            &at_end,
        );
        assert!(!actions.contains(&Action::SetPause(false)), "{actions:?}");
    }

    // Rule 4: position jumps while playing.

    #[test]
    fn steady_playback_does_not_seek() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(false);
        sync.tick(frame(t0, 0), &playing_at(10.0, t0), 0.0, &mpv);
        for n in 3..120 {
            // Host blocks arrive with a few ms of jitter.
            let jitter = Duration::from_millis(u64::from(n % 4));
            let at = frame(t0, n) - jitter;
            let pos = 10.0 + at.duration_since(t0).as_secs_f64();
            let actions = sync.tick(frame(t0, n), &playing_at(pos, at), 0.0, &mpv);
            assert!(seeks(&actions).is_empty(), "tick {n}: {actions:?}");
        }
    }

    #[test]
    fn seeks_on_a_loop_jump() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(false);
        sync.tick(frame(t0, 0), &playing_at(19.95, t0), 0.0, &mpv);
        sync.tick(frame(t0, 3), &playing_at(20.0, frame(t0, 3)), 0.0, &mpv);
        // The loop wraps from 20 s back to 4 s.
        let actions = sync.tick(frame(t0, 6), &playing_at(4.0, frame(t0, 6)), 0.0, &mpv);
        assert_eq!(seek_reasons(&actions), ["position jumped"]);
        assert_close(&seeks(&actions), &[4.0]);
    }

    #[test]
    fn holds_frame_zero_until_the_video_starts() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        // Offset 2 s: the video starts at song time 2 s.
        let actions = sync.tick(frame(t0, 0), &playing_at(1.9, t0), 2.0, &loaded(true));
        assert_close(&seeks(&actions), &[0.0]);
        assert!(!actions.contains(&Action::SetPause(false)));

        let at = t0 + Duration::from_millis(100);
        let actions = sync.tick(at, &playing_at(2.0, at), 2.0, &loaded(true));
        assert_eq!(seek_reasons(&actions), ["video starts"]);
        assert_eq!(actions.last(), Some(&Action::SetPause(false)));
    }

    // Rule 3: drift.

    #[test]
    fn checks_drift_twice_per_second_after_settling() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let mpv = loaded(false);
        let mut checks = Vec::new();
        for n in 0..120 {
            let at = frame(t0, n);
            let actions = sync.tick(at, &playing_at(f64::from(n) / 60.0, at), 0.0, &mpv);
            if actions.contains(&Action::CheckDrift) {
                checks.push(n);
            }
        }
        // The start seek settles for 300 ms, then a check every 500 ms. At 60 Hz each wait ends
        // on the first tick at or after the deadline.
        assert_eq!(checks.len(), 4, "{checks:?}");
        assert_eq!(checks[0], 19, "{checks:?}");
        assert!(checks.windows(2).all(|w| w[1] - w[0] == 31), "{checks:?}");
    }

    fn reading(time_pos: f64, at: Instant) -> TimePosReading {
        TimePosReading {
            time_pos,
            sent: at,
            received: at,
        }
    }

    #[test]
    fn frame_quantised_time_pos_in_sync_does_not_seek() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let transport = playing_at(10.0, t0);
        // 30 ms into frame 240 (10.000 s), mpv shows frame 240.
        let at = t0 + Duration::from_millis(30);
        let check = sync
            .on_time_pos(&reading(10.0, at), &transport, 0.0, Some(24.0))
            .unwrap();
        assert_eq!(check.error_frames, 0);
        assert!(!check.seek);
        // One frame off is still within tolerance.
        let check = sync
            .on_time_pos(&reading(10.042, t0), &transport, 0.0, Some(24.0))
            .unwrap();
        assert_eq!(check.error_frames, 1);
        assert!(!check.seek);
        assert_eq!(sync.pending_seek, None);
    }

    #[test]
    fn two_frames_of_drift_seeks() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let transport = playing_at(10.0, t0);
        let check = sync
            .on_time_pos(&reading(9.917, t0), &transport, 0.0, Some(24.0))
            .unwrap();
        assert_eq!(check.error_frames, -2);
        assert!(check.seek);
        assert_eq!(sync.pending_seek, Some("drift"));
    }

    #[test]
    fn drift_uses_the_query_midpoint_and_offset() {
        let t0 = Instant::now();
        let mut sync = Sync::default();
        let transport = playing_at(12.0, t0);
        let reading = TimePosReading {
            time_pos: 2.5,
            sent: t0 + Duration::from_millis(500),
            received: t0 + Duration::from_millis(502),
        };
        let check = sync.on_time_pos(&reading, &transport, 10.0, None).unwrap();
        assert!((check.expected - 2.501).abs() < 1e-9);
        assert!((check.error_seconds + 0.001).abs() < 1e-9);
    }
}
