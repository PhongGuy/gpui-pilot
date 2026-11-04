//! Builders for the pointer and key input the pilot sends, and helpers that dispatch it to a
//! window as if the platform had delivered it.

use gpui_kit::{
    App, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent,
    Window, point, px,
};
pub use gpui_pilot_protocol::{MAX_NOTCHES, NOTCH_MS_RANGE};

/// Wraps `events` in ModifiersChanged events that press `modifiers` first and release them all
/// last, as scroll does; GPUI keeps the last event's modifiers as the window's, so the release
/// must come last. With none held the events are returned as they are.
fn holding(modifiers: Modifiers, events: Vec<PlatformInput>) -> Vec<PlatformInput> {
    if modifiers == Modifiers::default() {
        return events;
    }
    let mut held = vec![changed_to(modifiers)];
    held.extend(events);
    held.push(changed_to(Modifiers::default()));
    held
}

/// The platform's report that the held modifiers are now `modifiers`.
fn changed_to(modifiers: Modifiers) -> PlatformInput {
    PlatformInput::ModifiersChanged(ModifiersChangedEvent {
        modifiers,
        ..Default::default()
    })
}

/// A pointer move to `at`, then one press and release per click with a rising click count,
/// so `count = 2` reads as a double click. Every event carries `modifiers`, which are held
/// from before the move until after the last release.
pub fn click_events(
    at: Point<Pixels>,
    button: MouseButton,
    count: usize,
    modifiers: Modifiers,
) -> Vec<PlatformInput> {
    let mut events = vec![PlatformInput::MouseMove(MouseMoveEvent {
        position: at,
        modifiers,
        ..Default::default()
    })];
    for click_count in 1..=count {
        events.push(PlatformInput::MouseDown(MouseDownEvent {
            button,
            position: at,
            modifiers,
            click_count,
            ..Default::default()
        }));
        events.push(PlatformInput::MouseUp(MouseUpEvent {
            button,
            position: at,
            modifiers,
            click_count,
        }));
    }
    holding(modifiers, events)
}

/// One keystroke per character, shaped like a US layout would report it: a space is the
/// `space` key, an ASCII capital is its lowercase key with shift, and `key_char` carries the
/// character the app should insert.
pub fn text_keystrokes(text: &str) -> Vec<Keystroke> {
    text.chars()
        .map(|c| {
            let (key, shift) = match c {
                ' ' => ("space".to_string(), false),
                c if c.is_ascii_uppercase() => (c.to_ascii_lowercase().to_string(), true),
                c => (c.to_string(), false),
            };
            Keystroke {
                modifiers: Modifiers {
                    shift,
                    ..Default::default()
                },
                key,
                key_char: Some(c.to_string()),
            }
        })
        .collect()
}

/// Space-separated GPUI keystrokes such as `ctrl-s escape`; the error names the first invalid
/// one, and an empty sequence is an error too.
pub fn parse_keys(keys: &str) -> Result<Vec<Keystroke>, String> {
    let parsed = keys
        .split_whitespace()
        .map(|key| Keystroke::parse(key).map_err(|error| format!("{key:?}: {error}")))
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        return Err("no keys were given".into());
    }
    Ok(parsed)
}

/// A pointer move to `at`, then one exact-pixel wheel event; with modifiers, wrapped in
/// ModifiersChanged events that press them first and release them all after. Positive `dy`
/// reveals content further down, which GPUI reads as a negative delta.
pub fn scroll_events(
    at: Point<Pixels>,
    dx: f32,
    dy: f32,
    modifiers: Modifiers,
) -> Vec<PlatformInput> {
    let moved = PlatformInput::MouseMove(MouseMoveEvent {
        position: at,
        modifiers,
        ..Default::default()
    });
    let wheel = PlatformInput::ScrollWheel(ScrollWheelEvent {
        position: at,
        delta: ScrollDelta::Pixels(point(px(-dx), px(-dy))),
        modifiers,
        ..Default::default()
    });
    if modifiers == Modifiers::default() {
        return vec![moved, wheel];
    }
    // GPUI keeps the last event's modifiers as the window's, so the release must come last.
    let changed = |modifiers| {
        PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            ..Default::default()
        })
    };
    vec![
        changed(modifiers),
        moved,
        wheel,
        changed(Modifiers::default()),
    ]
}

/// The events of one notch scroll at `at`: `press` holds `modifiers` (if any) and moves the
/// pointer there, `notches` is one `ScrollDelta::Lines` wheel event per notch (horizontal
/// notches first, each carrying `modifiers`), `release` lets the modifiers go.
#[non_exhaustive]
pub struct NotchEvents {
    pub press: Vec<PlatformInput>,
    pub notches: Vec<PlatformInput>,
    pub release: Vec<PlatformInput>,
}

/// Splits a notch scroll into its press, per-notch and release events; see `NotchEvents`. A
/// notch is three lines against the wheel direction, and with shift held a vertical notch
/// turns horizontal, as GPUI's Wayland client reports it.
pub fn notch_events(
    at: Point<Pixels>,
    notches_x: i32,
    notches_y: i32,
    modifiers: Modifiers,
) -> NotchEvents {
    let changed = |modifiers| {
        PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            ..Default::default()
        })
    };
    let held = modifiers != Modifiers::default();
    let mut press = Vec::new();
    if held {
        press.push(changed(modifiers));
    }
    press.push(PlatformInput::MouseMove(MouseMoveEvent {
        position: at,
        modifiers,
        ..Default::default()
    }));
    let wheel = |lines: Point<f32>| {
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: at,
            delta: ScrollDelta::Lines(lines),
            modifiers,
            ..Default::default()
        })
    };
    let step = |count: i32| -3.0 * count.signum() as f32;
    let horizontal = (0..notches_x.unsigned_abs()).map(|_| point(step(notches_x), 0.));
    let vertical = (0..notches_y.unsigned_abs()).map(|_| {
        if modifiers.shift {
            point(step(notches_y), 0.)
        } else {
            point(0., step(notches_y))
        }
    });
    NotchEvents {
        press,
        notches: horizontal.chain(vertical).map(wheel).collect(),
        release: if held {
            vec![changed(Modifiers::default())]
        } else {
            Vec::new()
        },
    }
}

/// One pointer move to `at`, which hover styles and tooltips react to.
pub fn hover_events(at: Point<Pixels>) -> Vec<PlatformInput> {
    vec![PlatformInput::MouseMove(MouseMoveEvent {
        position: at,
        ..Default::default()
    })]
}

/// A pointer move to `at`, then a `button` press there: the first half of a drag. `modifiers`
/// are pressed first and stay held through `drag_move` until `drag_release` lets them go.
pub fn drag_press(
    at: Point<Pixels>,
    button: MouseButton,
    modifiers: Modifiers,
) -> Vec<PlatformInput> {
    let mut events = vec![
        PlatformInput::MouseMove(MouseMoveEvent {
            position: at,
            modifiers,
            ..Default::default()
        }),
        PlatformInput::MouseDown(MouseDownEvent {
            button,
            position: at,
            modifiers,
            click_count: 1,
            ..Default::default()
        }),
    ];
    if modifiers != Modifiers::default() {
        events.insert(0, changed_to(modifiers));
    }
    events
}

/// A pointer move to `at` with `button` held; with the left one it starts and carries a GPUI
/// drag.
pub fn drag_move(at: Point<Pixels>, button: MouseButton, modifiers: Modifiers) -> PlatformInput {
    PlatformInput::MouseMove(MouseMoveEvent {
        position: at,
        pressed_button: Some(button),
        modifiers,
    })
}

/// The `button` release that drops a drag at `at`, then the release of its `modifiers`.
pub fn drag_release(
    at: Point<Pixels>,
    button: MouseButton,
    modifiers: Modifiers,
) -> Vec<PlatformInput> {
    let mut events = vec![PlatformInput::MouseUp(MouseUpEvent {
        button,
        position: at,
        modifiers,
        click_count: 1,
    })];
    if modifiers != Modifiers::default() {
        events.push(changed_to(Modifiers::default()));
    }
    events
}

/// `max(steps, 1)` evenly spaced points after `from`; the last is exactly `to`.
pub fn drag_path(from: Point<Pixels>, to: Point<Pixels>, steps: u32) -> Vec<Point<Pixels>> {
    let steps = steps.max(1);
    (1..=steps)
        .map(|i| {
            if i == steps {
                return to;
            }
            let t = i as f32 / steps as f32;
            point(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t)
        })
        .collect()
}

/// Delivers each event to the window in order.
pub fn dispatch_events(window: &mut Window, cx: &mut App, events: Vec<PlatformInput>) {
    for event in events {
        window.dispatch_event(event, cx);
    }
}

/// The modifiers a keystroke of only modifiers holds (`shift`, `ctrl-shift`), or `None` when
/// it names any other key. `Keystroke::parse` turns a lone modifier into the key itself.
fn modifier_only(keystroke: &Keystroke) -> Option<Modifiers> {
    let mut held = keystroke.modifiers;
    match keystroke.key.as_str() {
        "shift" => held.shift = true,
        "control" => held.control = true,
        "alt" => held.alt = true,
        "platform" => held.platform = true,
        "function" => held.function = true,
        _ => return None,
    }
    Some(held)
}

/// Presses and releases each keystroke in order; an unbound key inserts its `key_char` as
/// text. The release follows its press with no presented frame between, since a focused
/// element's keyboard click fires on the key-up of a key-down it saw. A keystroke of only
/// modifiers is a modifier change and its release, as Linux platforms deliver it, so it never
/// enters keyboard modality, which would hide hover and tooltips.
pub fn dispatch_keystrokes(window: &mut Window, cx: &mut App, keystrokes: Vec<Keystroke>) {
    for keystroke in keystrokes {
        if let Some(held) = modifier_only(&keystroke) {
            window.dispatch_event(changed_to(held), cx);
            window.dispatch_event(changed_to(Modifiers::default()), cx);
            continue;
        }
        window.dispatch_keystroke(keystroke.clone(), cx);
        window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        click_events, drag_move, drag_path, drag_press, drag_release, modifier_only, parse_keys,
        scroll_events, text_keystrokes,
    };
    use gpui_kit::{Modifiers, MouseButton, PlatformInput, ScrollDelta, point, px};

    #[test]
    fn a_click_moves_then_presses_and_releases_at_the_point() {
        let at = point(px(10.), px(20.));
        let events = click_events(at, MouseButton::Left, 2, Modifiers::default());
        assert_eq!(events.len(), 5);
        let PlatformInput::MouseMove(moved) = &events[0] else {
            panic!("the first event moves the pointer: {:?}", events[0]);
        };
        assert_eq!(moved.position, at);
        for (i, pair) in events[1..].chunks(2).enumerate() {
            let (PlatformInput::MouseDown(down), PlatformInput::MouseUp(up)) = (&pair[0], &pair[1])
            else {
                panic!("expected a press and a release: {pair:?}");
            };
            assert_eq!((down.position, up.position), (at, at));
            assert_eq!(
                (down.button, up.button),
                (MouseButton::Left, MouseButton::Left)
            );
            assert_eq!((down.click_count, up.click_count), (i + 1, i + 1));
        }
    }

    #[test]
    fn text_becomes_one_keystroke_per_character() {
        let keys = text_keystrokes("aB é");
        let seen: Vec<_> = keys
            .iter()
            .map(|k| (k.key.as_str(), k.key_char.as_deref(), k.modifiers.shift))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("a", Some("a"), false),
                ("b", Some("B"), true),
                ("space", Some(" "), false),
                ("é", Some("é"), false),
            ]
        );
        assert!(
            keys.iter()
                .all(|k| !k.modifiers.control && !k.modifiers.alt && !k.modifiers.platform)
        );
    }

    #[test]
    fn keys_parse_as_a_space_separated_sequence() {
        let keys = parse_keys("ctrl-s escape").unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys[0].modifiers.control);
        assert_eq!(
            (keys[0].key.as_str(), keys[1].key.as_str()),
            ("s", "escape")
        );
        let error = parse_keys("escape ctrl-a-b").unwrap_err();
        assert!(error.contains("ctrl-a-b"), "{error}");
    }

    #[test]
    fn only_a_keystroke_of_modifiers_alone_is_a_modifier_change() {
        let held = |keys: &str| modifier_only(&parse_keys(keys).unwrap()[0]);
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(held("shift"), Some(shift));
        assert_eq!(
            held("ctrl-shift"),
            Some(Modifiers {
                control: true,
                ..shift
            })
        );
        for keys in ["alt", "super", "cmd", "fn", "ctrl"] {
            assert!(held(keys).is_some(), "{keys}");
        }
        for keys in ["a", "ctrl-c", "shift-tab", "escape", "f13"] {
            assert_eq!(held(keys), None, "{keys}");
        }
    }

    #[test]
    fn scrolling_down_is_a_negative_wheel_delta() {
        let at = point(px(5.), px(6.));
        let events = scroll_events(at, 0.0, 120.0, Modifiers::default());
        let Some(PlatformInput::ScrollWheel(wheel)) = events.last() else {
            panic!("the last event scrolls: {events:?}");
        };
        assert_eq!(wheel.position, at);
        let ScrollDelta::Pixels(delta) = wheel.delta else {
            panic!("an exact pixel delta: {:?}", wheel.delta);
        };
        assert_eq!(delta, point(px(0.), px(-120.)));
    }

    #[test]
    fn a_modified_scroll_presses_moves_wheels_and_releases() {
        use gpui_kit::Modifiers;
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let events = scroll_events(point(px(5.), px(6.)), 0., 40., ctrl);
        let [
            PlatformInput::ModifiersChanged(down),
            PlatformInput::MouseMove(moved),
            PlatformInput::ScrollWheel(wheel),
            PlatformInput::ModifiersChanged(up),
        ] = events.as_slice()
        else {
            panic!("{events:?}")
        };
        assert_eq!(
            (down.modifiers, moved.modifiers, wheel.modifiers),
            (ctrl, ctrl, ctrl)
        );
        assert_eq!(up.modifiers, Modifiers::default());
        let plain = scroll_events(point(px(5.), px(6.)), 0., 40., Modifiers::default());
        assert_eq!(plain.len(), 2, "a plain scroll stays a move and a wheel");
    }

    #[test]
    fn a_drag_path_ends_at_the_target_in_even_steps() {
        let path = drag_path(point(px(0.), px(0.)), point(px(100.), px(50.)), 4);
        assert_eq!(
            path,
            vec![
                point(px(25.), px(12.5)),
                point(px(50.), px(25.)),
                point(px(75.), px(37.5)),
                point(px(100.), px(50.)),
            ]
        );
        assert_eq!(
            drag_path(point(px(1.), px(2.)), point(px(3.), px(4.)), 0),
            vec![point(px(3.), px(4.))],
            "zero steps still ends at the target"
        );
    }

    #[test]
    fn a_drag_presses_moves_with_the_button_held_and_releases() {
        let at = point(px(5.), px(6.));
        let press = drag_press(at, MouseButton::Left, Modifiers::default());
        assert!(
            matches!(&press[..], [PlatformInput::MouseMove(m), PlatformInput::MouseDown(d)]
                if m.position == at && m.pressed_button.is_none()
                    && d.position == at && d.button == MouseButton::Left && d.click_count == 1),
            "{press:?}"
        );
        let PlatformInput::MouseMove(moved) =
            drag_move(at, MouseButton::Left, Modifiers::default())
        else {
            panic!("a drag move is a mouse move");
        };
        assert_eq!(
            (moved.position, moved.pressed_button),
            (at, Some(MouseButton::Left))
        );
        let release = drag_release(at, MouseButton::Left, Modifiers::default());
        assert!(
            matches!(&release[..], [PlatformInput::MouseUp(u)]
                if u.position == at && u.button == MouseButton::Left && u.click_count == 1),
            "{release:?}"
        );
    }

    #[test]
    fn a_right_drag_presses_holds_and_releases_the_right_button() {
        let at = point(px(5.), px(6.));
        let right = MouseButton::Right;
        let press = drag_press(at, right, Modifiers::default());
        assert!(
            matches!(&press[..], [PlatformInput::MouseMove(_), PlatformInput::MouseDown(d)]
                if d.button == right),
            "{press:?}"
        );
        let PlatformInput::MouseMove(moved) = drag_move(at, right, Modifiers::default()) else {
            panic!("a drag move is a mouse move");
        };
        assert_eq!(moved.pressed_button, Some(right));
        let release = drag_release(at, right, Modifiers::default());
        assert!(
            matches!(&release[..], [PlatformInput::MouseUp(u)] if u.button == right),
            "{release:?}"
        );
    }

    #[test]
    fn a_modifier_click_carries_them_on_every_event_and_releases_them_last() {
        let at = point(px(5.), px(6.));
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let events = click_events(at, MouseButton::Left, 1, shift);
        let [
            PlatformInput::ModifiersChanged(held),
            PlatformInput::MouseMove(moved),
            PlatformInput::MouseDown(down),
            PlatformInput::MouseUp(up),
            PlatformInput::ModifiersChanged(released),
        ] = events.as_slice()
        else {
            panic!("press, move, down, up, release: {events:?}");
        };
        assert_eq!(
            (
                held.modifiers,
                moved.modifiers,
                down.modifiers,
                up.modifiers
            ),
            (shift, shift, shift, shift)
        );
        assert_eq!(released.modifiers, Modifiers::default());
    }

    #[test]
    fn a_modifier_drag_holds_them_from_the_press_to_the_release() {
        let at = point(px(5.), px(6.));
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let press = drag_press(at, MouseButton::Left, ctrl);
        let [
            PlatformInput::ModifiersChanged(held),
            PlatformInput::MouseMove(_),
            PlatformInput::MouseDown(down),
        ] = press.as_slice()
        else {
            panic!("hold, move, down: {press:?}");
        };
        assert_eq!((held.modifiers, down.modifiers), (ctrl, ctrl));
        let PlatformInput::MouseMove(moved) = drag_move(at, MouseButton::Left, ctrl) else {
            panic!("a drag move is a mouse move");
        };
        assert_eq!(moved.modifiers, ctrl);
        let release = drag_release(at, MouseButton::Left, ctrl);
        let [
            PlatformInput::MouseUp(up),
            PlatformInput::ModifiersChanged(released),
        ] = release.as_slice()
        else {
            panic!("up, release: {release:?}");
        };
        assert_eq!(
            (up.modifiers, released.modifiers),
            (ctrl, Modifiers::default())
        );
    }

    fn lines(event: &PlatformInput) -> gpui_kit::Point<f32> {
        match event {
            PlatformInput::ScrollWheel(gpui_kit::ScrollWheelEvent {
                delta: ScrollDelta::Lines(lines),
                ..
            }) => *lines,
            other => panic!("a line wheel event: {other:?}"),
        }
    }

    #[test]
    fn a_notch_is_three_lines_against_the_wheel_direction() {
        let at = point(px(5.), px(6.));
        let events = super::notch_events(at, 1, -2, Modifiers::default());
        assert_eq!(events.press.len(), 1, "only a move: {:?}", events.press);
        assert!(events.release.is_empty());
        let deltas: Vec<_> = events.notches.iter().map(lines).collect();
        assert_eq!(deltas, [point(-3., 0.), point(0., 3.), point(0., 3.)]);
        for event in &events.notches {
            let PlatformInput::ScrollWheel(wheel) = event else {
                unreachable!()
            };
            assert_eq!(wheel.position, at);
        }
    }

    #[test]
    fn shift_turns_a_vertical_notch_horizontal_and_is_released_after() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let events = super::notch_events(point(px(1.), px(1.)), 0, 1, shift);
        assert_eq!(events.press.len(), 2, "{:?}", events.press);
        assert!(matches!(
            events.press[0],
            PlatformInput::ModifiersChanged(_)
        ));
        assert_eq!(
            events.notches.iter().map(lines).collect::<Vec<_>>(),
            [point(-3., 0.)]
        );
        let PlatformInput::ScrollWheel(wheel) = &events.notches[0] else {
            unreachable!()
        };
        assert_eq!(wheel.modifiers, shift);
        let [PlatformInput::ModifiersChanged(released)] = events.release.as_slice() else {
            panic!("one release: {:?}", events.release);
        };
        assert_eq!(released.modifiers, Modifiers::default());
    }
}
