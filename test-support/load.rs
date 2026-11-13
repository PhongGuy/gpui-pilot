//! Machine load for timing assertions. A budget test that misses prints this beside its numbers,
//! so a miss on a busy machine reads differently from a regression. Included by `#[path]` from
//! each test crate's `common` module, since integration tests cannot share a library.
#![allow(dead_code)] // each including test binary uses a different subset

/// Renders `/proc/loadavg` text and a CPU count as one line, flagging a one-minute load above
/// the CPU count, where a timing miss is more likely load than a regression.
pub fn describe(loadavg: &str, cpus: usize) -> String {
    let mut fields = loadavg.split_whitespace();
    let (one, five, fifteen) = (fields.next(), fields.next(), fields.next());
    let (Some(one), Some(five), Some(fifteen)) = (one, five, fifteen) else {
        return format!("load unreadable ({cpus} cpus)");
    };
    let busy = one.parse::<f64>().is_ok_and(|load| load > cpus as f64);
    let verdict = if busy {
        "overloaded: a miss here is probably load, rerun serially"
    } else {
        "not overloaded: a miss here is probably a regression"
    };
    format!("load {one} {five} {fifteen} on {cpus} cpus, {verdict}")
}

/// The current machine load, for appending to a timing assertion's message.
pub fn report() -> String {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let text = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    describe(&text, cpus)
}

/// Whether a `GPUI_PILOT_SKIP_TIMING` value asks timing tests to skip: exactly `1`.
pub fn skips_timing(value: Option<&str>) -> bool {
    value == Some("1")
}

/// True, after saying so on stderr, when `GPUI_PILOT_SKIP_TIMING=1`. CI sets it: a shared
/// runner misses frame and time budgets for reasons unrelated to the code.
pub fn timing_skipped(test: &str) -> bool {
    let value = std::env::var("GPUI_PILOT_SKIP_TIMING").ok();
    let skip = skips_timing(value.as_deref());
    if skip {
        eprintln!("{test}: timing skipped (GPUI_PILOT_SKIP_TIMING=1)");
    }
    skip
}
