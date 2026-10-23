//! Screenshot settle policy.
//!
//! KWin's ScreenCast stream delivers a frame on damage or cursor motion only.
//! A screenshot taken right after an action must not return the newest frame
//! that happens to be queued: that is often the cursor-only frame of the
//! action's own pointer motion, from before the application repainted. So a
//! capture collects new frames until none arrives for `SETTLE_QUIET` (or
//! `SETTLE_DEADLINE` passed since the capture began) and uses the last one.
//! On a static screen nothing arrives and the stored frame - already current -
//! is used after the deadline.
//!
//! The policy is pure over a non-blocking `poll` callback, so how frames reach
//! the daemon (today: lamco-pipewire's frame channel, one frame per process
//! callback) can change without touching it. Waiting is async, so the
//! daemon's SIGTERM handling stays live during a capture.

use std::time::{Duration, Instant};

/// Quiet gap after the last new frame that counts as "settled".
pub const SETTLE_QUIET: Duration = Duration::from_millis(70);
/// Upper bound for the whole settle wait.
pub const SETTLE_DEADLINE: Duration = Duration::from_millis(300);

/// What one poll of the frame source found.
pub enum Arrival<T> {
    /// A new frame of the stream being captured.
    Target(T),
    /// A frame of some other stream (already stored by the callback); it does
    /// not restart the quiet clock.
    Other,
    /// Nothing available right now.
    Nothing,
}

/// Collect frames from `poll` (non-blocking; drained until `Nothing`, then
/// re-polled every `interval`) until the target stream has been quiet for
/// `quiet` after its last frame, or `deadline` has passed; return the last
/// target frame, `None` when none arrived at all.
pub async fn settle<T>(
    quiet: Duration,
    deadline: Duration,
    interval: Duration,
    mut poll: impl FnMut() -> Arrival<T>,
) -> Option<T> {
    let end = Instant::now() + deadline;
    let mut latest: Option<(T, Instant)> = None;
    loop {
        loop {
            match poll() {
                Arrival::Target(frame) => latest = Some((frame, Instant::now())),
                Arrival::Other => {}
                Arrival::Nothing => break,
            }
        }
        let until = match &latest {
            None => end,
            Some((_, arrived)) => end.min(*arrived + quiet),
        };
        let now = Instant::now();
        if now >= until {
            break;
        }
        tokio::time::sleep((until - now).min(interval)).await;
    }
    latest.map(|(frame, _)| frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame source that makes `script` items available at fixed offsets
    /// from its creation.
    struct Scripted {
        start: Instant,
        script: Vec<(u64, Option<u32>)>,
    }

    impl Scripted {
        fn new(mut script: Vec<(u64, Option<u32>)>) -> Self {
            script.reverse();
            Self {
                start: Instant::now(),
                script,
            }
        }

        fn poll(&mut self) -> Arrival<u32> {
            match self.script.last() {
                Some(&(at_ms, item))
                    if self.start + Duration::from_millis(at_ms) <= Instant::now() =>
                {
                    self.script.pop();
                    match item {
                        Some(frame) => Arrival::Target(frame),
                        None => Arrival::Other,
                    }
                }
                _ => Arrival::Nothing,
            }
        }
    }

    const QUIET: Duration = Duration::from_millis(70);
    const DEADLINE: Duration = Duration::from_millis(300);
    const POLL: Duration = Duration::from_millis(5);

    #[tokio::test]
    async fn a_burst_settles_on_its_last_frame_after_the_quiet_gap() {
        // Cursor-only frame first, the application's repaint 40 ms later.
        let mut source = Scripted::new(vec![(5, Some(1)), (25, Some(2)), (45, Some(3))]);
        let started = Instant::now();
        let got = settle(QUIET, DEADLINE, POLL, || source.poll()).await;
        let took = started.elapsed();
        assert_eq!(got, Some(3));
        assert!(took >= Duration::from_millis(45 + 60), "{took:?}");
        assert!(took < Duration::from_millis(250), "{took:?}");
    }

    #[tokio::test]
    async fn a_static_screen_yields_nothing_after_the_deadline() {
        let mut source = Scripted::new(vec![]);
        let started = Instant::now();
        assert_eq!(settle(QUIET, DEADLINE, POLL, || source.poll()).await, None);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(290), "{took:?}");
        assert!(took < Duration::from_millis(400), "{took:?}");
    }

    #[tokio::test]
    async fn a_continuous_stream_is_cut_off_at_the_deadline() {
        let script = (1..=20).map(|i| (i * 30, Some(i as u32))).collect();
        let mut source = Scripted::new(script);
        let started = Instant::now();
        let got = settle(QUIET, DEADLINE, POLL, || source.poll())
            .await
            .unwrap();
        let took = started.elapsed();
        assert!((8..=10).contains(&got), "{got}");
        assert!(took < Duration::from_millis(400), "{took:?}");
    }

    #[tokio::test]
    async fn other_streams_do_not_restart_the_quiet_clock() {
        // Target frame at 10 ms, other-stream frames every 20 ms after it:
        // settled 70 ms after the target frame, not at the deadline.
        let mut script = vec![(10, Some(7))];
        script.extend((1..=12).map(|i| (10 + i * 20, None)));
        let mut source = Scripted::new(script);
        let started = Instant::now();
        assert_eq!(
            settle(QUIET, DEADLINE, POLL, || source.poll()).await,
            Some(7)
        );
        let took = started.elapsed();
        assert!(took < Duration::from_millis(200), "{took:?}");
    }

    #[tokio::test]
    async fn a_late_first_frame_is_still_taken_before_the_deadline() {
        let mut source = Scripted::new(vec![(200, Some(9))]);
        assert_eq!(
            settle(QUIET, DEADLINE, POLL, || source.poll()).await,
            Some(9)
        );
    }
}
