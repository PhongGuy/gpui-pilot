mod common;

use common::{bounds_of, first_frame, with_demo};
use gpui_kit::px;

#[test]
fn renders_pilot_demo_offscreen_at_scale_one() {
    let (width, height, button, background) = with_demo(640., 400., 1.0, |mut h| async move {
        let frame = first_frame(&h).await;
        let count = bounds_of(&mut h, "count");
        let x = (f32::from(count.left()) + 4.0) as u32;
        let y = f32::from(count.center().y) as u32;
        (
            frame.image.width(),
            frame.image.height(),
            *frame.image.get_pixel(x, y),
            *frame.image.get_pixel(2, 2),
        )
    });
    assert_eq!((width, height), (640, 400));
    assert_ne!(
        button, background,
        "the Count button paints over the background"
    );
}

#[test]
fn launch_scale_multiplies_device_pixels_not_logical_bounds() {
    let (width, height, count) = with_demo(320., 200., 2.0, |mut h| async move {
        let frame = first_frame(&h).await;
        let count = bounds_of(&mut h, "count");
        (frame.image.width(), frame.image.height(), count)
    });
    assert_eq!((width, height), (640, 400));
    assert!(count.right() <= px(320.), "bounds stay logical: {count:?}");
    assert!(count.size.height > px(0.));
}
