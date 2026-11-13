//! The headless frame clock: absolute deadlines at the launch rate, observed on an idle window.

mod common;

use std::time::{Duration, Instant};

use common::{first_frame, with_demo_at};
use futures::StreamExt as _;

/// The first `n` distinct ticker wakes of the idle demo window, as `(deadline, woke)`.
fn wakes(refresh_hz: u32, n: usize) -> Vec<(Instant, Instant)> {
    with_demo_at(640., 400., 1.0, refresh_hz, move |h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        let mut ticks = window.ticks();
        let mut wakes: Vec<(Instant, Instant)> = Vec::new();
        while wakes.len() < n {
            ticks.next().await.expect("the window stays open");
            if let Some(wake) = window.last_wake()
                && wakes.last() != Some(&wake)
            {
                wakes.push(wake);
            }
        }
        wakes
    })
}

#[test]
fn the_ticker_wakes_on_its_deadlines_at_every_rate() {
    if common::load::timing_skipped("the_ticker_wakes_on_its_deadlines_at_every_rate") {
        return;
    }
    for hz in [60, 144, 240] {
        let wakes = wakes(hz, 120);
        let period = Duration::from_nanos(1_000_000_000 / u64::from(hz));
        let mut errors: Vec<Duration> = wakes
            .iter()
            .map(|(deadline, woke)| {
                woke.checked_duration_since(*deadline)
                    .unwrap_or_else(|| panic!("{hz} Hz: woke before its deadline"))
            })
            .collect();
        errors.sort();
        let median = errors[errors.len() / 2];
        eprintln!(
            "{hz} Hz: wake error median {median:?} max {:?}",
            errors.last().unwrap()
        );
        assert!(
            median < Duration::from_micros(500),
            "{hz} Hz: median {median:?}"
        );
        // Consecutive deadlines sit on the grid: whole periods apart, within a nanosecond.
        for pair in wakes.windows(2) {
            let gap = (pair[1].0 - pair[0].0).as_nanos() as f64;
            let periods = gap / period.as_nanos() as f64;
            assert!(
                periods >= 0.999 && (periods - periods.round()).abs() < 1e-3,
                "{hz} Hz: gap {gap} ns"
            );
        }
    }
}
