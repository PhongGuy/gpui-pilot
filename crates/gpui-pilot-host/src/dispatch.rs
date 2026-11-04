//! `CountingDispatcher`: the platform dispatcher with a count of the work it still owes the app,
//! so settle can tell an idle app from one with queued tasks, busy polls or near timers.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_kit::{
    ActivityGuard, PlatformDispatcher, Priority, RunnableVariant, TimerResolutionGuard,
};

/// Default settle horizon: timers due later than this are not pending work.
const DEFAULT_HORIZON_MS: u64 = 250;

/// Wraps the Linux dispatcher. Main-thread runnables count until they start, background
/// runnables until their poll returns, and timers due within the horizon until they have fired.
pub(crate) struct CountingDispatcher {
    inner: Arc<dyn PlatformDispatcher>,
    pending: Arc<AtomicUsize>,
    horizon_ms: AtomicU64,
}

/// Holds one unit of pending work and gives it back when dropped, so a runnable that is
/// discarded unrun or panics while running cannot leave the count stuck.
struct Held(Arc<AtomicUsize>);

impl Held {
    fn new(pending: &Arc<AtomicUsize>) -> Self {
        pending.fetch_add(1, Ordering::SeqCst);
        Self(pending.clone())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CountingDispatcher {
    pub(crate) fn new(inner: Arc<dyn PlatformDispatcher>) -> Self {
        Self {
            inner,
            pending: Arc::default(),
            horizon_ms: AtomicU64::new(DEFAULT_HORIZON_MS),
        }
    }

    pub(crate) fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    pub(crate) fn set_horizon(&self, horizon: Duration) {
        let ms = u64::try_from(horizon.as_millis()).unwrap_or(u64::MAX);
        self.horizon_ms.store(ms, Ordering::SeqCst);
    }

    /// Re-wraps `runnable` so it stays counted until it starts (`until_done` false) or until
    /// its poll returns (`until_done` true), then hands the wrapper to `forward`.
    fn counted(
        &self,
        runnable: RunnableVariant,
        until_done: bool,
        forward: impl Fn(RunnableVariant) + Send + Sync + 'static,
    ) {
        let held = Held::new(&self.pending);
        let meta = runnable.metadata().clone();
        let (wrapper, task) = async_task::Builder::new().metadata(meta).spawn(
            move |_| async move {
                if until_done {
                    runnable.run();
                    drop(held);
                } else {
                    drop(held);
                    runnable.run();
                }
            },
            forward,
        );
        wrapper.schedule();
        task.detach();
    }
}

impl PlatformDispatcher for CountingDispatcher {
    fn is_main_thread(&self) -> bool {
        self.inner.is_main_thread()
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        let inner = self.inner.clone();
        self.counted(runnable, true, move |w| inner.dispatch(w, priority));
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        let inner = self.inner.clone();
        self.counted(runnable, false, move |w| {
            inner.dispatch_on_main_thread(w, priority)
        });
    }

    // A timer stays counted until its runnable has woken the waiter, whose own wakeup is
    // counted before this one is released. Timers due beyond the horizon (a blinking cursor)
    // must not hold settle off forever.
    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        if duration > Duration::from_millis(self.horizon_ms.load(Ordering::SeqCst)) {
            return self.inner.dispatch_after(duration, runnable);
        }
        let inner = self.inner.clone();
        self.counted(runnable, true, move |w| inner.dispatch_after(duration, w));
    }

    // `dispatch_on_main_thread_when_idle` keeps the trait default, which routes through the
    // counted `dispatch_on_main_thread`; forwarding it would bypass the count.

    fn idle_time_remaining(&self) -> Option<Duration> {
        self.inner.idle_time_remaining()
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        self.inner.spawn_realtime(f)
    }

    fn now(&self) -> Instant {
        self.inner.now()
    }

    fn increase_timer_resolution(&self) -> TimerResolutionGuard {
        self.inner.increase_timer_resolution()
    }

    fn prevent_app_nap(&self, reason: &str) -> ActivityGuard {
        self.inner.prevent_app_nap(reason)
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::time::Duration;

    use gpui_kit::{PlatformDispatcher, Priority, RunnableMeta, RunnableVariant};

    use super::CountingDispatcher;

    /// Discards background work, runs main-thread work at once and survives its panics, and
    /// fires timers immediately.
    struct Careless;

    impl PlatformDispatcher for Careless {
        fn is_main_thread(&self) -> bool {
            true
        }
        fn dispatch(&self, runnable: RunnableVariant, _priority: Priority) {
            drop(runnable);
        }
        fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
            catch_unwind(AssertUnwindSafe(|| runnable.run())).ok();
        }
        fn dispatch_after(&self, _duration: Duration, runnable: RunnableVariant) {
            runnable.run();
        }
        fn spawn_realtime(&self, _f: Box<dyn FnOnce() + Send>) {}
    }

    fn runnable(body: impl FnOnce() + Send + 'static) -> RunnableVariant {
        let (runnable, task) = async_task::Builder::new()
            .metadata(RunnableMeta::new_with_callers_location())
            .spawn(move |_| async move { body() }, |_| {});
        task.detach();
        runnable
    }

    #[test]
    fn a_runnable_dropped_unrun_releases_its_count() {
        let dispatcher = CountingDispatcher::new(Arc::new(Careless));
        dispatcher.dispatch(runnable(|| {}), Priority::default());
        assert_eq!(dispatcher.pending(), 0);
    }

    #[test]
    fn a_panicking_runnable_releases_its_count() {
        let dispatcher = CountingDispatcher::new(Arc::new(Careless));
        let panics = runnable(|| panic!("the app's task panicked"));
        dispatcher.dispatch_on_main_thread(panics, Priority::default());
        assert_eq!(dispatcher.pending(), 0);
    }

    #[test]
    fn a_fired_timer_releases_its_count() {
        let dispatcher = CountingDispatcher::new(Arc::new(Careless));
        dispatcher.dispatch_after(Duration::from_millis(10), runnable(|| {}));
        assert_eq!(dispatcher.pending(), 0);
    }
}
