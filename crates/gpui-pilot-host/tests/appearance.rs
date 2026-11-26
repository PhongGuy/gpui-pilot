mod common;

use common::{Harness, first_frame, with_demo, with_demo_in};
use gpui_kit::{Platform, WindowAppearance};

/// What the window, the app and the platform report, in that order.
async fn seen(mut h: Harness) -> (WindowAppearance, WindowAppearance, WindowAppearance) {
    first_frame(&h).await;
    let window = h
        .window
        .update(&mut h.cx, |_, window, _| window.appearance())
        .expect("the demo window is open");
    let app = h.cx.update(|cx| cx.window_appearance());
    let platform = Platform::window_appearance(&*h.platform);
    (window, app, platform)
}

#[test]
fn a_light_launch_is_light_to_the_window_the_app_and_the_platform() {
    let seen = with_demo_in(WindowAppearance::Light, seen);
    assert_eq!(
        seen,
        (
            WindowAppearance::Light,
            WindowAppearance::Light,
            WindowAppearance::Light
        )
    );
}

#[test]
fn a_default_launch_is_dark_everywhere() {
    let seen = with_demo(640., 400., 1.0, seen);
    assert_eq!(
        seen,
        (
            WindowAppearance::Dark,
            WindowAppearance::Dark,
            WindowAppearance::Dark
        ),
        "the window and the platform agree"
    );
}
