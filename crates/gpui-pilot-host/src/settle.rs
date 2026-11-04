//! Settle: wait until the app has no pending work and its pixels have stopped changing, or,
//! while an animation keeps repainting, until its observed layout has stopped changing.

use std::{
    pin::pin,
    time::{Duration, Instant},
};

use futures::{
    StreamExt as _,
    future::{Either, select},
};
use gpui_kit::{AppContext as _, AsyncApp};

use crate::{PilotPlatform, PilotWindow, observe};

/// How long pixels must stay still on an idle window before it counts as settled.
pub const IDLE_MS: u64 = 32;

/// How long the observed layout must stay unchanged before an animating window counts as
/// settled.
pub const QUIET_LAYOUT_MS: u64 = 160;

/// The number of frame ticks at `refresh_hz` that span `ms`: ceil(ms * refresh_hz / 1000).
pub fn ticks_for(ms: u64, refresh_hz: u32) -> u32 {
    let ticks = (ms * u64::from(refresh_hz)).div_ceil(1000);
    u32::try_from(ticks).unwrap_or(u32::MAX)
}

/// Tracks frame ticks for one settle wait.
pub struct SettleTracker {
    last: Option<u64>,
    quiet: u32,
    quiet_layout: u32,
    animating: bool,
    idle_ticks: u32,
    quiet_layout_ticks: u32,
}

impl SettleTracker {
    /// `baseline` is the hash of the frame on screen when the wait starts; `refresh_hz`
    /// converts `IDLE_MS` and `QUIET_LAYOUT_MS` into tick counts.
    pub fn new(baseline: Option<u64>, refresh_hz: u32) -> Self {
        Self {
            last: baseline,
            quiet: 0,
            quiet_layout: 0,
            animating: false,
            idle_ticks: ticks_for(IDLE_MS, refresh_hz),
            quiet_layout_ticks: ticks_for(QUIET_LAYOUT_MS, refresh_hz),
        }
    }

    /// Feeds one tick; true once `IDLE_MS` of consecutive ticks were idle and drew nothing
    /// new, or once `QUIET_LAYOUT_MS` of consecutive ticks were idle with `layout_same`: this
    /// tick's observed snapshots equal the previous tick's.
    pub fn observe(&mut self, drawn: Option<u64>, idle: bool, layout_same: bool) -> bool {
        let new_pixels = drawn.is_some_and(|hash| self.last != Some(hash));
        if let Some(hash) = drawn {
            self.last = Some(hash);
        }
        self.quiet = if idle && !new_pixels {
            self.quiet + 1
        } else {
            0
        };
        self.quiet_layout = if idle && layout_same {
            self.quiet_layout + 1
        } else {
            0
        };
        // Still pixels are the stronger evidence, so they win when both rules hold.
        if self.quiet >= self.idle_ticks {
            self.animating = false;
            return true;
        }
        self.animating = self.quiet_layout >= self.quiet_layout_ticks;
        self.animating
    }

    /// True when the last `true` from `observe` came from the quiet-layout rule.
    pub fn animating(&self) -> bool {
        self.animating
    }
}

/// The outcome of a settle wait: whether it settled within the budget, how long it took, how
/// many frame ticks it saw, and whether it settled on a quiet layout under an animation.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Settled {
    pub settled: bool,
    pub elapsed: Duration,
    pub ticks: u32,
    pub animating: bool,
}

/// Waits on the window's frame ticks until `IDLE_MS` of them in a row find no pending work, no
/// failed render and no new pixels, or `QUIET_LAYOUT_MS` of them find no pending work and an
/// unchanged layout, counted in ticks at the platform's refresh rate. A spent `budget` or a
/// closed window yields `settled: false`.
pub async fn settle(
    platform: &PilotPlatform,
    window: &PilotWindow,
    cx: &AsyncApp,
    budget: Duration,
) -> Settled {
    // `update_window` needs `&mut`, and the caller lends this context only shared.
    let mut cx = cx.clone();
    let handle = window.handle();
    let layout = |cx: &mut AsyncApp| cx.update_window(handle, |_, window, _| observe(window));
    let started = Instant::now();
    // A zero budget never sees the ticks settling needs, so it answers without asking for pixels.
    if budget.is_zero() {
        return Settled {
            settled: false,
            elapsed: started.elapsed(),
            ticks: 0,
            animating: false,
        };
    }
    let mut ticks = window.ticks();
    // A failed readback leaves no baseline; its `render_error` keeps every tick from idling.
    let baseline = window.last_frame().ok().flatten().map(|frame| frame.hash());
    let mut tracker = SettleTracker::new(baseline, platform.refresh_hz());
    let mut deadline = pin!(platform.pilot_timers().timer(budget));
    let mut seen = 0;
    let outcome = |settled, ticks, animating| Settled {
        settled,
        elapsed: started.elapsed(),
        ticks,
        animating,
    };
    let Ok(mut previous) = layout(&mut cx) else {
        return outcome(false, seen, false);
    };
    loop {
        let drawn = match select(ticks.next(), deadline.as_mut()).await {
            Either::Left((Some(drawn), _)) => drawn,
            // The window closed or the budget ran out.
            Either::Left((None, _)) | Either::Right(_) => return outcome(false, seen, false),
        };
        seen += 1;
        // This task is running, not queued, so it does not count itself.
        let idle = platform.pending_work() == 0 && window.render_error().is_none();
        let Ok(current) = layout(&mut cx) else {
            return outcome(false, seen, false);
        };
        let layout_same = current == previous;
        previous = current;
        if tracker.observe(drawn, idle, layout_same) {
            return outcome(true, seen, tracker.animating());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SettleTracker;

    /// Feeds `(drawn, idle)` ticks and returns `observe`'s answer after each.
    fn run(baseline: Option<u64>, ticks: &[(Option<u64>, bool)]) -> Vec<bool> {
        let mut tracker = SettleTracker::new(baseline, 60);
        ticks
            .iter()
            .map(|&(drawn, idle)| tracker.observe(drawn, idle, false))
            .collect()
    }

    #[test]
    fn two_quiet_idle_ticks_settle() {
        assert_eq!(run(Some(1), &[(None, true), (None, true)]), [false, true]);
        // A re-presented identical frame is quiet.
        let same = [(Some(7), true), (Some(7), true)];
        assert_eq!(run(Some(7), &same), [false, true]);
    }

    #[test]
    fn new_pixels_or_queued_work_restart_the_count() {
        let changed = [(Some(2), true), (Some(2), true), (None, true)];
        assert_eq!(run(Some(1), &changed), [false, false, true]);
        let busy = [(None, true), (None, false), (None, true), (None, true)];
        assert_eq!(run(Some(1), &busy), [false, false, false, true]);
        let first = [(Some(3), true), (None, true), (Some(3), true)];
        assert_eq!(run(None, &first), [false, false, true]);
    }

    #[test]
    fn a_frame_that_keeps_changing_never_settles() {
        let ticks: Vec<_> = (0..100).map(|hash| (Some(hash), true)).collect();
        assert!(run(None, &ticks).iter().all(|settled| !settled));
    }

    #[test]
    fn changing_pixels_over_a_quiet_layout_settle_as_animating() {
        let mut tracker = SettleTracker::new(None, 60);
        let settled_at = (0..100u64).position(|hash| tracker.observe(Some(hash), true, true));
        assert_eq!(
            settled_at,
            Some(super::ticks_for(super::QUIET_LAYOUT_MS, 60) as usize - 1)
        );
        assert!(tracker.animating());
    }

    #[test]
    fn pending_work_or_a_layout_change_restarts_the_layout_count() {
        let mut tracker = SettleTracker::new(None, 60);
        let mut hash = 0u64..;
        let quiet = super::ticks_for(super::QUIET_LAYOUT_MS, 60) - 1;
        for _ in 0..quiet {
            assert!(!tracker.observe(hash.next(), true, true));
        }
        assert!(
            !tracker.observe(hash.next(), false, true),
            "pending work is not quiet"
        );
        for _ in 0..quiet {
            assert!(!tracker.observe(hash.next(), true, true));
        }
        assert!(
            !tracker.observe(hash.next(), true, false),
            "a layout change is not quiet"
        );
        for _ in 0..quiet {
            assert!(!tracker.observe(hash.next(), true, true));
        }
        assert!(tracker.observe(hash.next(), true, true));
    }

    #[test]
    fn a_pixel_settle_is_not_animating() {
        let mut tracker = SettleTracker::new(Some(1), 60);
        assert!(!tracker.observe(None, true, true));
        assert!(tracker.observe(None, true, true));
        assert!(!tracker.animating());
    }

    #[test]
    fn thresholds_keep_their_wall_clock_length_at_every_rate() {
        use super::{IDLE_MS, QUIET_LAYOUT_MS, ticks_for};
        let at = |hz| (ticks_for(IDLE_MS, hz), ticks_for(QUIET_LAYOUT_MS, hz));
        assert_eq!(at(60), (2, 10), "60 Hz keeps today's counts");
        assert_eq!(at(120), (4, 20));
        assert_eq!(at(144), (5, 24));
        assert_eq!(at(240), (8, 39));
    }

    #[test]
    fn at_144_hz_five_quiet_ticks_settle() {
        let mut tracker = SettleTracker::new(Some(1), 144);
        let settled: Vec<bool> = (0..5).map(|_| tracker.observe(None, true, false)).collect();
        assert_eq!(settled, [false, false, false, false, true]);
    }
}
