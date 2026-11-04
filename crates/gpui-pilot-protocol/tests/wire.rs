use std::io::Cursor;
use std::time::{Duration, Instant};

use gpui_pilot_protocol::{
    Answered, BackendFrame, BuildInfo, ClientFrame, DEFAULT_REFRESH_HZ, DialogInfo, DialogLevel,
    ErrorCode, FrameError, LaunchParams, Modifier, MouseButton, Node, Outcome, PROTOCOL_VERSION,
    PickAnswer, PilotError, PromptInfo, PromptKind, REFRESH_RATES_HZ, Recorder, Rect, Reply,
    Request, Segment, Target, WaitUntil, Wheel, WindowInfo, read_line, write_line,
};
use serde_json::{Value, json};

fn round_trip(request: Request) {
    let text = serde_json::to_string(&request).unwrap();
    assert_eq!(
        serde_json::from_str::<Request>(&text).unwrap(),
        request,
        "{text}"
    );
}

#[test]
fn targets_take_exactly_one_shape() {
    let id: Target = serde_json::from_value(json!({"id": ["panel", 3]})).unwrap();
    assert_eq!(
        id,
        Target::Id {
            id: vec![Segment::Str("panel".into()), Segment::Int(3)]
        }
    );
    let role: Target = serde_json::from_value(json!({"role": "button", "name": "Count"})).unwrap();
    assert!(matches!(role, Target::Role { nth: None, .. }));
    let point: Target = serde_json::from_value(json!({"x": 1.5, "y": 2})).unwrap();
    assert_eq!(point, Target::Point { x: 1.5, y: 2.0 });
    assert!(serde_json::from_value::<Target>(json!({"id": ["a"], "x": 1, "y": 2})).is_err());
    assert!(serde_json::from_value::<Target>(json!({"role": "b", "name": "c", "z": 0})).is_err());
}

#[test]
fn a_click_request_has_its_documented_shape() {
    let click = Request::Click {
        target: Target::Id {
            id: vec![Segment::Str("count".into())],
        },
        button: MouseButton::Left,
        count: 1,
        modifiers: Vec::new(),
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    let expected = json!({"op": "click", "target": {"id": ["count"]}, "button": "left", "count": 1, "timeout_ms": 5000, "settle_ms": 2000});
    assert_eq!(serde_json::to_value(&click).unwrap(), expected);
    round_trip(click);
    round_trip(Request::Key {
        keys: "ctrl-s escape".into(),
        settle_ms: 2000,
    });
    round_trip(Request::PickFile {
        answer: PickAnswer::Cancel,
        settle_ms: 0,
        timeout_ms: None,
    });
    round_trip(Request::Close);
}

#[test]
fn version_two_shapes_for_hover_wait_and_hit() {
    let hover = Request::Hover {
        target: Target::Point { x: 1.0, y: 2.0 },
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&hover).unwrap(),
        json!({"op": "hover", "target": {"x": 1.0, "y": 2.0}, "timeout_ms": 5000, "settle_ms": 2000})
    );
    round_trip(hover);
    let visible = Request::Wait {
        until: WaitUntil::Visible(Target::Id {
            id: vec![Segment::Str("late".into())],
        }),
        timeout_ms: 5000,
    };
    assert_eq!(
        serde_json::to_value(&visible).unwrap(),
        json!({"op": "wait", "until": {"visible": {"id": ["late"]}}, "timeout_ms": 5000})
    );
    round_trip(visible);
    round_trip(Request::Wait {
        until: WaitUntil::Gone(Target::Role {
            role: "menu".into(),
            name: "x".into(),
            nth: None,
        }),
        timeout_ms: 0,
    });
    let elapsed = Request::Wait {
        until: WaitUntil::Elapsed(250),
        timeout_ms: 250,
    };
    assert_eq!(
        serde_json::to_value(&elapsed).unwrap()["until"],
        json!({"elapsed": 250})
    );
    round_trip(elapsed);
    round_trip(Request::Tree {
        within: None,
        wait_for: None,
        timeout_ms: 0,
        settle_ms: 1000,
    });

    let node = Node::new(
        vec![Segment::Str("cover".into())],
        Rect::new(0.0, 0.0, 9.0, 9.0),
        true,
        0,
    );
    let mut occluded = PilotError::new(ErrorCode::Occluded, "covered");
    occluded.hit = Some(node.clone());
    let value = serde_json::to_value(Outcome::Err(occluded)).unwrap();
    assert_eq!(value["err"]["code"], json!("Occluded"));
    assert_eq!(value["err"]["hit"]["path"], json!(["cover"]));
    let disabled = serde_json::to_value(PilotError::new(ErrorCode::Disabled, "off")).unwrap();
    assert!(disabled.get("hit").is_none());
    assert_eq!(
        serde_json::to_value(ErrorCode::InvalidArgument).unwrap(),
        json!("InvalidArgument")
    );
    let click = Reply::Click {
        clicked: node.clone(),
        hit: node,
        settled: true,
    };
    assert_eq!(
        serde_json::to_value(&click).unwrap()["hit"]["path"],
        json!(["cover"])
    );
    let waited = serde_json::to_value(Reply::Wait {
        node: None,
        waited_ms: 7,
    })
    .unwrap();
    assert_eq!(waited, json!({"op": "wait", "waited_ms": 7}));
    let hello = Reply::Hello {
        version: 2,
        pid: 1,
        window: WindowInfo::new(1.0, 2.0, 1.0),
        settled: true,
        refresh_mhz: 60_000,
    };
    assert_eq!(
        serde_json::to_value(&hello).unwrap()["settled"],
        json!(true)
    );
    let launch = LaunchParams::new(1.0, 1.0, 1.0, "/d".into(), Vec::new());
    assert_eq!(launch.settle_ms, 5000);
}

#[test]
fn unknown_request_fields_are_rejected() {
    let bad = json!({"op": "tree", "timeout_ms": 5, "within": null, "wait_for": null, "bogus": 1});
    assert!(serde_json::from_value::<Request>(bad).is_err());
}

#[test]
fn pick_answers_and_error_codes_serialise_by_name() {
    let paths = serde_json::to_value(PickAnswer::Paths(vec!["/a".into()])).unwrap();
    assert_eq!(paths, json!({"paths": ["/a"]}));
    assert_eq!(
        serde_json::to_value(PickAnswer::Cancel).unwrap(),
        json!("cancel")
    );
    assert_eq!(
        serde_json::to_value(Answered::Queued).unwrap(),
        json!("queued")
    );
    let mut error = PilotError::new(ErrorCode::ElementNotFound, "no element `cont`");
    error.nearest = vec![vec![Segment::Str("count".into())]];
    let value = serde_json::to_value(Outcome::Err(error)).unwrap();
    assert_eq!(value["err"]["code"], json!("ElementNotFound"));
    assert_eq!(value["err"]["nearest"], json!([["count"]]));
    assert!(value["err"].get("matches").is_none());
    let ok = serde_json::to_value(Outcome::Ok(Reply::Type {
        settled: true,
        masked: false,
    }))
    .unwrap();
    assert_eq!(ok, json!({"ok": {"op": "type", "settled": true}}));
}

/// `masked` is optional both ways: an older host's reply without it reads as unmasked, and a
/// masked reply only adds the one field, which an older server ignores.
#[test]
fn a_type_reply_carries_masked_only_when_set() {
    let old: Reply = serde_json::from_value(json!({"op": "type", "settled": true})).unwrap();
    assert_eq!(
        old,
        Reply::Type {
            settled: true,
            masked: false
        }
    );
    let masked = serde_json::to_value(Reply::Type {
        settled: false,
        masked: true,
    })
    .unwrap();
    assert_eq!(
        masked,
        json!({"op": "type", "settled": false, "masked": true})
    );
}

#[test]
fn a_node_omits_absent_optional_fields() {
    let node = Node::new(
        vec![Segment::Int(1)],
        Rect::new(1.0, 2.0, 3.0, 4.0),
        true,
        0,
    );
    let value = serde_json::to_value(&node).unwrap();
    let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["bounds", "depth", "path", "visible"]);
    assert_eq!(
        value["bounds"],
        json!({"x": 1.0, "y": 2.0, "w": 3.0, "h": 4.0})
    );
}

#[test]
fn frames_are_json_lines_and_end_cleanly() {
    let mut wire = Vec::new();
    write_line(&mut wire, &ClientFrame::new(1, Request::Close)).unwrap();
    write_line(&mut wire, &BackendFrame::new(1, Outcome::Ok(Reply::Closed))).unwrap();
    assert_eq!(wire.iter().filter(|b| **b == b'\n').count(), 2);
    let mut input = Cursor::new(wire);
    let first: ClientFrame = read_line(&mut input).unwrap().unwrap();
    assert_eq!((first.id, first.request), (1, Request::Close));
    let second: Value = read_line(&mut input).unwrap().unwrap();
    assert_eq!(
        second,
        json!({"id": 1, "outcome": {"ok": {"op": "closed"}}})
    );
    assert!(read_line::<Value>(&mut input).unwrap().is_none());
    let mut junk = Cursor::new(b"{not json\n".to_vec());
    assert!(matches!(
        read_line::<Value>(&mut junk),
        Err(FrameError::Json(_))
    ));
}

#[test]
fn pick_file_carries_a_timeout_only_when_set() {
    let headless = Request::PickFile {
        answer: PickAnswer::Cancel,
        settle_ms: 0,
        timeout_ms: None,
    };
    assert_eq!(
        serde_json::to_value(&headless).unwrap(),
        json!({"op": "pick_file", "answer": "cancel", "settle_ms": 0}),
        "a protocol 2 host sees the same frame"
    );
    let older: Request =
        serde_json::from_value(json!({"op": "pick_file", "answer": "cancel", "settle_ms": 0}))
            .unwrap();
    assert_eq!(older, headless);
    let session = Request::PickFile {
        answer: PickAnswer::Paths(vec!["/tmp/a".into()]),
        settle_ms: 0,
        timeout_ms: Some(10_000),
    };
    assert_eq!(
        serde_json::to_value(&session).unwrap()["timeout_ms"],
        json!(10_000)
    );
    round_trip(session);
}

#[test]
fn version_three_adds_id_suffix_and_tree_dialog_state() {
    let suffix: Target = serde_json::from_value(json!({"id_suffix": "json.yml"})).unwrap();
    assert_eq!(
        suffix,
        Target::IdSuffix {
            id_suffix: "json.yml".into()
        }
    );
    assert_eq!(
        serde_json::to_value(&suffix).unwrap(),
        json!({"id_suffix": "json.yml"})
    );
    assert!(serde_json::from_value::<Target>(json!({"id_suffix": "a", "id": ["a"]})).is_err());
    let bare = Reply::Tree {
        nodes: vec![],
        prompt: None,
        pick_queued: false,
        dialog: None,
    };
    assert_eq!(
        serde_json::to_value(&bare).unwrap(),
        json!({"op": "tree", "nodes": []})
    );
    assert_eq!(
        serde_json::from_value::<Reply>(json!({"op": "tree", "nodes": []})).unwrap(),
        bare,
        "a protocol 2 tree reply still parses"
    );
    let waiting = Reply::Tree {
        nodes: vec![],
        prompt: Some(PromptInfo::new(PromptKind::Open, false)),
        pick_queued: true,
        dialog: None,
    };
    let value = serde_json::to_value(&waiting).unwrap();
    assert_eq!(value["prompt"], json!({"kind": "open", "multiple": false}));
    assert_eq!(value["pick_queued"], json!(true));
    assert_eq!(serde_json::from_value::<Reply>(value).unwrap(), waiting);
}

#[test]
fn version_four_adds_drag_and_dialogs() {
    let drag = Request::Drag {
        from: Target::Id {
            id: vec![Segment::Str("chip".into())],
        },
        to: Target::Point { x: 10.0, y: 20.0 },
        steps: 10,
        button: MouseButton::Left,
        modifiers: Vec::new(),
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&drag).unwrap(),
        json!({"op": "drag", "from": {"id": ["chip"]}, "to": {"x": 10.0, "y": 20.0},
            "steps": 10, "timeout_ms": 5000, "settle_ms": 2000})
    );
    round_trip(drag);
    let read = Request::Dialog {
        button: None,
        timeout_ms: 5000,
        settle_ms: 1000,
    };
    assert_eq!(
        serde_json::to_value(&read).unwrap(),
        json!({"op": "dialog", "button": null, "timeout_ms": 5000, "settle_ms": 1000})
    );
    round_trip(read);
    let mut dialog = DialogInfo::new(
        DialogLevel::Warning,
        "Delete the chip?",
        vec!["Delete".into(), "Cancel".into()],
    );
    dialog.detail = Some("This cannot be undone.".into());
    let tree = Reply::Tree {
        nodes: vec![],
        prompt: None,
        pick_queued: false,
        dialog: Some(dialog.clone()),
    };
    let value = serde_json::to_value(&tree).unwrap();
    assert_eq!(
        value["dialog"],
        json!({"level": "warning", "message": "Delete the chip?",
            "detail": "This cannot be undone.", "buttons": ["Delete", "Cancel"]})
    );
    assert_eq!(serde_json::from_value::<Reply>(value).unwrap(), tree);
    let none = Reply::Dialog {
        dialog: None,
        answered: None,
        settled: false,
    };
    assert_eq!(
        serde_json::to_value(&none).unwrap(),
        json!({"op": "dialog", "settled": false})
    );
    let answered = Reply::Dialog {
        dialog: Some(dialog),
        answered: Some("Delete".into()),
        settled: true,
    };
    let value = serde_json::to_value(&answered).unwrap();
    assert_eq!(serde_json::from_value::<Reply>(value).unwrap(), answered);
}

#[test]
fn version_five_adds_scroll_modifiers_and_window_close() {
    let scroll = Request::Scroll {
        target: Target::Point { x: 1.0, y: 2.0 },
        dx: 0.0,
        dy: 40.0,
        modifiers: vec![Modifier::Ctrl, Modifier::Shift],
        wheel: None,
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&scroll).unwrap(),
        json!({"op": "scroll", "target": {"x": 1.0, "y": 2.0}, "dx": 0.0, "dy": 40.0,
               "modifiers": ["ctrl", "shift"], "timeout_ms": 5000, "settle_ms": 2000})
    );
    round_trip(scroll);
    // Without modifiers a scroll keeps its protocol 4 shape both ways.
    let plain: Request = serde_json::from_value(json!({"op": "scroll", "target": {"x": 1, "y": 2},
        "dx": 0, "dy": 40, "timeout_ms": 5000, "settle_ms": 2000}))
    .unwrap();
    let Request::Scroll { modifiers, .. } = &plain else {
        panic!("{plain:?}")
    };
    assert!(modifiers.is_empty());
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("modifiers")
            .is_none()
    );
    for (name, modifier) in [
        ("ctrl", Modifier::Ctrl),
        ("alt", Modifier::Alt),
        ("shift", Modifier::Shift),
        ("super", Modifier::Super),
    ] {
        assert_eq!(serde_json::to_value(modifier).unwrap(), json!(name));
    }
    assert!(serde_json::from_value::<Modifier>(json!("hyper")).is_err());

    let close = Request::CloseWindow {
        timeout_ms: 2000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&close).unwrap(),
        json!({"op": "close_window", "timeout_ms": 2000, "settle_ms": 2000})
    );
    round_trip(close);
    assert_eq!(
        serde_json::to_value(Reply::CloseWindow {
            closed: false,
            settled: true
        })
        .unwrap(),
        json!({"op": "close_window", "closed": false, "settled": true})
    );
}

#[test]
fn version_six_tells_the_app_its_data_was_reused() {
    let mut launch = LaunchParams::new(640.0, 400.0, 1.0, "/d".into(), Vec::new());
    assert!(!launch.reused);
    assert!(
        serde_json::to_value(&launch)
            .unwrap()
            .get("reused")
            .is_none()
    );
    launch.reused = true;
    assert_eq!(
        serde_json::to_value(&launch).unwrap()["reused"],
        json!(true)
    );
    let back: LaunchParams =
        serde_json::from_value(serde_json::to_value(&launch).unwrap()).unwrap();
    assert!(back.reused);
}

#[test]
fn version_seven_carries_the_refresh_rate() {
    assert_eq!(REFRESH_RATES_HZ, [60, 120, 144, 240]);
    assert_eq!(DEFAULT_REFRESH_HZ, 144);
    let launch = LaunchParams::new(640.0, 400.0, 1.0, "/d".into(), Vec::new());
    assert_eq!(launch.refresh_hz, 144);
    let mut value = serde_json::to_value(&launch).unwrap();
    assert_eq!(value["refresh_hz"], json!(144));
    value.as_object_mut().unwrap().remove("refresh_hz");
    let back: LaunchParams = serde_json::from_value(value).unwrap();
    assert_eq!(back.refresh_hz, 144, "an absent rate means 144");
    let hello = Reply::Hello {
        version: 7,
        pid: 1,
        window: WindowInfo::new(1.0, 1.0, 1.0),
        settled: true,
        refresh_mhz: 143_832,
    };
    let value = serde_json::to_value(&hello).unwrap();
    assert_eq!(value["refresh_mhz"], json!(143_832));
    assert_eq!(serde_json::from_value::<Reply>(value).unwrap(), hello);
}

#[test]
fn version_seven_records_frames_and_sends_wheel_notches() {
    let start = Request::FramesStart { idle_ms: 100 };
    assert_eq!(
        serde_json::to_value(&start).unwrap(),
        json!({"op": "frames_start", "idle_ms": 100})
    );
    round_trip(start);
    assert_eq!(
        serde_json::to_value(Request::FramesStop).unwrap(),
        json!({"op": "frames_stop"})
    );
    round_trip(Request::FramesStop);
    let bogus = json!({"op": "frames_start", "idle_ms": 1, "bogus": 1});
    assert!(serde_json::from_value::<Request>(bogus).is_err());
    assert_eq!(
        serde_json::to_value(Reply::FramesStarted {
            refresh_mhz: 144_000
        })
        .unwrap(),
        json!({"op": "frames_started", "refresh_mhz": 144_000})
    );

    let t0 = Instant::now();
    let mut recorder = Recorder::new(t0, 144_000, 100);
    recorder.frame(
        t0 + Duration::from_millis(7),
        Some(Duration::from_millis(2)),
    );
    let report = recorder.finish(
        t0 + Duration::from_millis(10),
        Some(BuildInfo::new("0", true)),
    );
    let value = serde_json::to_value(Reply::Frames(report.clone())).unwrap();
    assert_eq!(
        (value["op"].clone(), value["frames"].clone()),
        (json!("frames"), json!(1))
    );
    assert!(
        value.get("intervals").is_some() && value["intervals"].is_null(),
        "{value}"
    );
    assert_eq!(
        value["build"],
        json!({"opt_level": "0", "debug_assertions": true})
    );
    assert_eq!(
        serde_json::from_value::<Reply>(value).unwrap(),
        Reply::Frames(report)
    );

    let scroll = |wheel| Request::Scroll {
        target: Target::Point { x: 1.0, y: 2.0 },
        dx: 0.0,
        dy: 0.0,
        modifiers: Vec::new(),
        wheel,
        timeout_ms: 5000,
        settle_ms: 0,
    };
    let notched = scroll(Some(Wheel::new(0, -3, 16)));
    assert_eq!(
        serde_json::to_value(&notched).unwrap()["wheel"],
        json!({"notches_x": 0, "notches_y": -3, "notch_ms": 16})
    );
    round_trip(notched);
    // Without notches the field is absent both ways, so a pixel scroll keeps its shape.
    assert!(
        serde_json::to_value(scroll(None))
            .unwrap()
            .get("wheel")
            .is_none()
    );
}

#[test]
fn notch_limits_are_shared_by_every_backend() {
    assert_eq!(gpui_pilot_protocol::MAX_NOTCHES, 200);
    assert_eq!(gpui_pilot_protocol::NOTCH_MS_RANGE, 1..=1000);
}

#[test]
fn version_eight_adds_click_and_drag_modifiers() {
    assert_eq!(gpui_pilot_protocol::MODIFIERS_VERSION, 8);
    let click = Request::Click {
        target: Target::Point { x: 1.0, y: 2.0 },
        button: MouseButton::Left,
        count: 1,
        modifiers: vec![Modifier::Ctrl, Modifier::Shift],
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&click).unwrap(),
        json!({"op": "click", "target": {"x": 1.0, "y": 2.0}, "button": "left", "count": 1,
               "modifiers": ["ctrl", "shift"], "timeout_ms": 5000, "settle_ms": 2000})
    );
    round_trip(click);
    let drag = Request::Drag {
        from: Target::Point { x: 1.0, y: 2.0 },
        to: Target::Point { x: 3.0, y: 4.0 },
        steps: 10,
        button: MouseButton::Left,
        modifiers: vec![Modifier::Alt],
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&drag).unwrap()["modifiers"],
        json!(["alt"])
    );
    round_trip(drag);
    // Without modifiers both keep their protocol 7 shape, in and out.
    for plain in [
        json!({"op": "click", "target": {"x": 1.0, "y": 2.0}, "button": "left", "count": 1,
               "timeout_ms": 5000, "settle_ms": 2000}),
        json!({"op": "drag", "from": {"x": 1.0, "y": 2.0}, "to": {"x": 3.0, "y": 4.0}, "steps": 10,
               "timeout_ms": 5000, "settle_ms": 2000}),
    ] {
        let request: Request = serde_json::from_value(plain.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), plain);
    }
}

#[test]
fn version_nine_adds_the_drag_button() {
    assert_eq!(gpui_pilot_protocol::DRAG_BUTTON_VERSION, 9);
}

#[test]
fn a_drag_names_its_button_only_when_it_is_not_left() {
    let drag = |button| Request::Drag {
        from: Target::Point { x: 1.0, y: 2.0 },
        to: Target::Point { x: 3.0, y: 4.0 },
        steps: 10,
        button,
        modifiers: Vec::new(),
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(drag(MouseButton::Right)).unwrap(),
        json!({"op": "drag", "from": {"x": 1.0, "y": 2.0}, "to": {"x": 3.0, "y": 4.0}, "steps": 10,
               "button": "right", "timeout_ms": 5000, "settle_ms": 2000})
    );
    round_trip(drag(MouseButton::Middle));
    // A left drag keeps the protocol 8 shape, so a host built before `button` still reads it.
    let left = serde_json::to_value(drag(MouseButton::Left)).unwrap();
    assert!(left.get("button").is_none(), "{left}");
    let read: Request = serde_json::from_value(left).unwrap();
    assert_eq!(read, drag(MouseButton::Left));
}

#[test]
fn a_lenient_read_returns_a_bad_frame_with_its_id_and_keeps_reading() {
    use gpui_pilot_protocol::{ClientFrame, read_line_lenient};
    let input = concat!(
        r#"{"id":7,"request":{"op":"key","keys":"a","settle_ms":0,"from_the_future":1}}"#,
        "\n",
        r#"{"id":8,"request":{"op":"close"}}"#,
        "\n",
    );
    let mut reader = std::io::BufReader::new(input.as_bytes());
    let bad = read_line_lenient::<ClientFrame>(&mut reader)
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(bad.id, Some(7));
    assert!(bad.message.contains("from_the_future"), "{}", bad.message);
    let next = read_line_lenient::<ClientFrame>(&mut reader)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(next.id, 8);
    assert!(
        read_line_lenient::<ClientFrame>(&mut reader)
            .unwrap()
            .is_none()
    );
}

#[test]
fn version_ten_adds_drop_with_explicit_fields() {
    assert_eq!(gpui_pilot_protocol::DROP_VERSION, 10);
    let drop = Request::Drop {
        target: Target::Point { x: 1.0, y: 2.0 },
        paths: vec!["/tmp/a".into(), "/tmp/b c".into()],
        cancel: false,
        timeout_ms: 5000,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&drop).unwrap(),
        json!({"op": "drop", "target": {"x": 1.0, "y": 2.0}, "paths": ["/tmp/a", "/tmp/b c"],
               "cancel": false, "timeout_ms": 5000, "settle_ms": 2000})
    );
    round_trip(drop);
    let node = Node::new(Vec::new(), Rect::new(0.0, 0.0, 1.0, 1.0), true, 0);
    let reply = Reply::Drop {
        over: node.clone(),
        hit: node,
        dropped: true,
        settled: true,
    };
    let text = serde_json::to_string(&reply).unwrap();
    assert!(text.starts_with(r#"{"op":"drop","over":"#), "{text}");
    assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), reply);
}

#[test]
fn version_eleven_adds_resize_with_the_size_the_window_has_after() {
    assert_eq!(PROTOCOL_VERSION, 11);
    assert_eq!(gpui_pilot_protocol::RESIZE_VERSION, 11);
    let resize = Request::Resize {
        width: 800.0,
        height: 600.0,
        settle_ms: 2000,
    };
    assert_eq!(
        serde_json::to_value(&resize).unwrap(),
        json!({"op": "resize", "width": 800.0, "height": 600.0, "settle_ms": 2000})
    );
    round_trip(resize);
    let reply = Reply::Resize {
        window: WindowInfo::new(800.0, 600.0, 1.5),
        settled: true,
    };
    let text = serde_json::to_string(&reply).unwrap();
    assert_eq!(
        text,
        r#"{"op":"resize","window":{"w":800.0,"h":600.0,"scale":1.5},"settled":true}"#
    );
    assert_eq!(serde_json::from_str::<Reply>(&text).unwrap(), reply);
}
