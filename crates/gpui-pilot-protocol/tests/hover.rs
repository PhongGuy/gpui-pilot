//! The hover wait both backends share.

use gpui_pilot_protocol::{TOOLTIP_DELAY_MS, hover_wait_ms};

#[test]
fn hover_waits_out_the_tooltip_delay_and_one_frame_within_the_settle_budget() {
    assert_eq!(TOOLTIP_DELAY_MS, 500);
    assert_eq!(hover_wait_ms(2000, 144_000), 507);
    assert_eq!(hover_wait_ms(2000, 143_838), 507);
    assert_eq!(hover_wait_ms(2000, 60_000), 517);
    assert_eq!(hover_wait_ms(300, 144_000), 300);
    assert_eq!(hover_wait_ms(0, 144_000), 0);
}

#[test]
fn an_unmeasured_refresh_rate_waits_the_whole_budget_instead_of_dividing_by_zero() {
    assert_eq!(hover_wait_ms(2000, 0), 2000);
    assert_eq!(hover_wait_ms(0, 0), 0);
}
