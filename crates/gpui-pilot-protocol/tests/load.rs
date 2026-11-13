//! The load line timing assertions append to their failure messages.

#[path = "../../../test-support/load.rs"]
mod load;

#[test]
fn a_load_above_the_cpu_count_is_called_overloaded() {
    let line = load::describe("25.10 20.00 12.50 3/900 1234\n", 16);
    assert_eq!(
        line,
        "load 25.10 20.00 12.50 on 16 cpus, overloaded: a miss here is probably load, rerun serially"
    );
}

#[test]
fn a_quiet_machine_points_at_a_regression() {
    let line = load::describe("0.40 0.50 0.60 1/300 99\n", 16);
    assert!(line.contains("not overloaded"), "{line}");
    assert!(line.contains("load 0.40 0.50 0.60 on 16 cpus"), "{line}");
}

#[test]
fn unreadable_load_is_named_not_guessed() {
    assert_eq!(load::describe("", 8), "load unreadable (8 cpus)");
    assert!(load::report().contains("cpus"));
}

#[test]
fn only_a_skip_timing_value_of_one_skips() {
    assert!(load::skips_timing(Some("1")));
    for value in [None, Some(""), Some("0"), Some("true"), Some(" 1")] {
        assert!(!load::skips_timing(value), "{value:?}");
    }
}
