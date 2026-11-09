//! The private session from the inside: KWin, geometry, capture, the AT-SPI tree and EIS input.

mod common;

use std::time::{Duration, Instant};

use common::{Demo, OUTPUT, inner, run_inside, session_tests};
use gpui_pilot_protocol::{Modifier, MouseButton, Rect, Wheel};
use gpui_pilot_session::Session;
use gpui_pilot_session::SessionError;
use gpui_pilot_session::sandbox::Output;

/// Runs `inner` inside the sandbox when the session suite is enabled.
fn outer(inner: &str, done: &str) {
    if !session_tests() {
        return;
    }
    let out = run_inside(inner, OUTPUT, |_| {});
    assert!(out.contains(done), "{out}");
}

#[test]
fn kwin_places_and_captures_the_demo() {
    outer("inner_places_and_captures", "session ok");
}

#[test]
#[ignore = "runs inside the sandbox through kwin_places_and_captures_the_demo"]
fn inner_places_and_captures() {
    if !inner() {
        return;
    }
    let mut session = Session::start(OUTPUT).unwrap();
    let demo = Demo::spawn();
    let first = session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    assert_eq!(first.scale, 1.5);
    let placed = session
        .place(demo.pid(), Rect::new(100., 60., 900., 600.))
        .unwrap();
    assert_eq!(placed.frame, Rect::new(100., 60., 900., 600.));
    assert_eq!((placed.client.x, placed.client.w), (100., 900.));
    assert!(
        placed.client.y > 60. && placed.client.y < 120.,
        "a title bar: {placed:?}"
    );
    assert_eq!(
        session.window(demo.pid()).unwrap(),
        placed,
        "a second script reads the same"
    );
    session.check_isolation().unwrap();

    let shot = session.capture().unwrap();
    assert_eq!((shot.width, shot.height, shot.scale), (1920, 1200, 1.5));
    assert_eq!(
        shot.pixel(30, 30)[..3],
        [0, 0, 0],
        "the bare desktop is black"
    );
    let inside = shot.pixel(
        ((100. + 450.) * 1.5) as u32,
        ((placed.client.y + 450.) * 1.5) as u32,
    );
    assert!(
        inside[..3].iter().all(|c| *c > 200),
        "the demo's blank area is light: {inside:?}"
    );
    let client = shot.crop(&placed.client);
    assert_eq!(
        (client.width, client.height),
        (1350, (placed.client.h * 1.5).round() as u32)
    );
    println!("session ok");
}

/// Reads the tree every 50 ms for up to 3 s until `ready` holds; panics with the last read.
fn settled_tree(
    session: &mut Session,
    pid: u32,
    ready: impl Fn(&[gpui_pilot_protocol::Node]) -> bool,
) -> Vec<gpui_pilot_protocol::Node> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let nodes = session.tree(pid).unwrap();
        if ready(&nodes) {
            return nodes;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the layout never settled: {nodes:#?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_tree_reads_ids_roles_and_logical_bounds() {
    outer("inner_tree", "tree ok");
}

#[test]
#[ignore = "runs inside the sandbox through the_tree_reads_ids_roles_and_logical_bounds"]
fn inner_tree() {
    if !inner() {
        return;
    }
    use gpui_pilot_protocol::Segment;
    let mut session = Session::start(OUTPUT).unwrap();
    let demo = Demo::spawn();
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let placed = session
        .place(demo.pid(), Rect::new(0., 0., 1280., 800.))
        .unwrap();
    // GPUI re-lays out a frame or more after KWin resizes it; poll until the count button
    // spans the new client width.
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes
            .iter()
            .any(|n| n.id.as_deref() == Some("count") && n.bounds.w == placed.client.w - 32.)
    });
    let by_id = |id: &str| {
        nodes
            .iter()
            .find(|n| n.id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no node {id} in {nodes:#?}"))
    };
    let count = by_id("count");
    assert_eq!(
        (count.role.as_deref(), count.name.as_deref()),
        (Some("Button"), Some("Count"))
    );
    assert_eq!(
        count.bounds,
        Rect::new(16., 50., 1248., 32.),
        "AT-SPI device px / 1.5"
    );
    assert_eq!(
        (count.path.len(), count.path.first()),
        (2, Some(&Segment::Int(0))),
        "a child of the frame"
    );
    assert!(count.visible && count.disabled.is_none());
    let name = by_id("name");
    assert_eq!(name.role.as_deref(), Some("Entry"));
    // Since gpui-kit 0.9 the id is on the Input's editor, inside its frame's 6 px padding.
    assert_eq!(name.bounds.y, 96.);
    let frame = nodes
        .iter()
        .find(|n| n.path == vec![Segment::Int(0)])
        .expect("the frame");
    assert_eq!(
        frame.bounds,
        Rect::new(0., 0., placed.client.w, placed.client.h)
    );
    assert!(nodes.iter().all(|n| n.depth as usize + 1 == n.path.len()));
    assert!(
        nodes
            .windows(2)
            .all(|w| (w[0].bounds.y, w[0].bounds.x) <= (w[1].bounds.y, w[1].bounds.x))
    );
    assert!(
        nodes
            .iter()
            .all(|n| n.role.as_deref() != Some("Application"))
    );
    println!("tree ok");
}

#[test]
fn eis_clicks_and_types_off_origin_at_scale_one_and_a_half() {
    outer("inner_gate", "gate ok");
}

#[test]
#[ignore = "runs inside the sandbox through eis_clicks_and_types_off_origin_at_scale_one_and_a_half"]
fn inner_gate() {
    if !inner() {
        return;
    }
    let mut session = Session::start(OUTPUT).unwrap();
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let placed = session
        .place(demo.pid(), Rect::new(100., 60., 900., 600.))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes
            .iter()
            .any(|n| n.id.as_deref() == Some("count") && n.bounds.w == placed.client.w - 32.)
    });
    let centre = |id: &str| {
        let b = &nodes
            .iter()
            .find(|n| n.id.as_deref() == Some(id))
            .unwrap()
            .bounds;
        (b.x + b.w / 2., b.y + b.h / 2.)
    };

    session
        .click(demo.pid(), centre("count"), MouseButton::Left, 1, &[])
        .unwrap();
    demo.wait_state("clicks=1 name=\"\"");
    session
        .click(demo.pid(), centre("name"), MouseButton::Left, 1, &[])
        .unwrap();
    demo.lines_for(Duration::from_millis(300));
    session.type_text("Hié").unwrap();
    demo.wait_state("clicks=1 name=\"Hié\"");

    let refused = session.type_text("a漢").unwrap_err();
    assert!(
        matches!(refused, SessionError::UnsupportedCharacter('漢')),
        "{refused}"
    );
    let after = demo.lines_for(Duration::from_millis(300));
    assert!(
        after.iter().all(|l| !l.contains("Hiéa")),
        "nothing was typed: {after:?}"
    );

    session.keys("backspace shift-a").unwrap();
    demo.wait_state("clicks=1 name=\"HiA\"");
    session.hover(demo.pid(), centre("count")).unwrap();
    session
        .scroll(demo.pid(), centre("name"), 0., 40., &[])
        .unwrap();
    session.check_isolation().unwrap();
    println!("gate ok");
}

#[test]
fn eis_scrolls_with_ctrl_held_and_releases_it() {
    outer("inner_ctrl_scroll", "zoom ok");
}

#[test]
#[ignore = "runs inside the sandbox through eis_scrolls_with_ctrl_held_and_releases_it"]
fn inner_ctrl_scroll() {
    if !inner() {
        return;
    }
    let mut session = Session::start(OUTPUT).unwrap();
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes.iter().any(|n| n.id.as_deref() == Some("name"))
    });
    let b = &nodes
        .iter()
        .find(|n| n.id.as_deref() == Some("name"))
        .unwrap()
        .bounds;
    let at = (b.x + b.w / 2., b.y + b.h / 2.);
    // 15 px is one 120-unit notch, so one wheel event.
    session
        .scroll(demo.pid(), at, 0., 15., &[Modifier::Ctrl])
        .unwrap();
    demo.wait_state("clicks=0 name=\"\" zoom=1");
    // A plain scroll afterwards is no zoom: ctrl was released.
    session.scroll(demo.pid(), at, 0., 15., &[]).unwrap();
    demo.wait_state("clicks=0 name=\"\" scrolled zoom=1");
    println!("zoom ok");
}

#[test]
fn kwin_closes_a_window_unless_the_app_refuses() {
    outer("inner_close_window", "close ok");
}

#[test]
#[ignore = "runs inside the sandbox through kwin_closes_a_window_unless_the_app_refuses"]
fn inner_close_window() {
    if !inner() {
        return;
    }
    let mut session = Session::start(OUTPUT).unwrap();
    let guarded = Demo::spawn_with(&[("PILOT_DEMO_GUARD_CLOSE", "1")]);
    guarded.wait_state("opened");
    session
        .wait_window(guarded.pid(), Duration::from_secs(15))
        .unwrap();
    let closed = session
        .close_window(guarded.pid(), Duration::from_secs(1))
        .unwrap();
    assert!(!closed, "the guarded demo refused the close");
    guarded.wait_state("clicks=0 name=\"\" close_asks=1");
    session
        .window(guarded.pid())
        .expect("the guarded window is still open");
    drop(guarded);

    let mut plain = Demo::spawn();
    plain.wait_state("opened");
    session
        .wait_window(plain.pid(), Duration::from_secs(15))
        .unwrap();
    assert!(
        session
            .close_window(plain.pid(), Duration::from_secs(5))
            .unwrap(),
        "an unguarded window closes"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while plain.child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the demo quits once its last window closed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("close ok");
}

#[test]
fn the_portal_watch_sees_the_demos_prompt_and_its_cancel() {
    outer("inner_portal_watch", "watch ok");
}

#[test]
#[ignore = "runs inside the sandbox through the_portal_watch_sees_the_demos_prompt_and_its_cancel"]
fn inner_portal_watch() {
    if !inner() {
        return;
    }
    use gpui_pilot_protocol::PromptKind;
    use gpui_pilot_session::portal::{PortalEvent, PortalWatch, pending};
    use std::time::Instant;
    let mut session = Session::start(OUTPUT).unwrap();
    let watch = PortalWatch::start().unwrap();
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes.iter().any(|n| n.name.as_deref() == Some("Open"))
    });
    let open = nodes
        .iter()
        .find(|n| n.role.as_deref() == Some("Button") && n.name.as_deref() == Some("Open"))
        .unwrap();
    let at = (
        open.bounds.x + open.bounds.w / 2.,
        open.bounds.y + open.bounds.h / 2.,
    );
    session
        .click(demo.pid(), at, MouseButton::Left, 1, &[])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let prompt = loop {
        if let Some((_, prompt)) = pending(&watch.events()) {
            break prompt;
        }
        assert!(
            Instant::now() < deadline,
            "no FileChooser call: {:?}",
            watch.events()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!((prompt.kind, prompt.multiple), (PromptKind::Open, false));
    // The dialog maps a few hundred ms after the call (spike §2) and then holds focus.
    std::thread::sleep(Duration::from_secs(1));
    session.keys("escape").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !watch
        .events()
        .iter()
        .any(|e| matches!(e, PortalEvent::Response { code: 1, .. }))
    {
        assert!(
            Instant::now() < deadline,
            "no cancel response: {:?}",
            watch.events()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("watch ok");
}

#[test]
fn the_portal_dialog_opens_saves_picks_folders_cancels_and_refuses() {
    outer("inner_dialog", "dialog ok");
}

#[test]
#[ignore = "runs inside the sandbox through the_portal_dialog_opens_saves_picks_folders_cancels_and_refuses"]
fn inner_dialog() {
    if !inner() {
        return;
    }
    use gpui_pilot_protocol::{Answered, PickAnswer, PromptKind};
    use std::path::{Path, PathBuf};
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let input = home.join("in put.txt");
    std::fs::write(&input, "fixture").unwrap();
    let folder = home.join("folder");
    std::fs::create_dir(&folder).unwrap();
    let target = home.join("saved.txt");
    let mut session = Session::start(OUTPUT).unwrap();
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes.iter().any(|n| n.name.as_deref() == Some("Folder"))
    });
    let centre = |name: &str| {
        let b = &nodes
            .iter()
            .find(|n| n.role.as_deref() == Some("Button") && n.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no button {name}"))
            .bounds;
        (b.x + b.w / 2., b.y + b.h / 2.)
    };
    let wait = Duration::from_secs(10);
    let press = |session: &mut Session, name: &str| {
        session
            .click(demo.pid(), centre(name), MouseButton::Left, 1, &[])
            .unwrap()
    };

    press(&mut session, "Open");
    let opened = session
        .answer_prompt(&PickAnswer::Paths(vec![input.clone()]), wait)
        .unwrap();
    assert_eq!(
        (opened.answered, opened.prompt.kind, opened.prompt.multiple),
        (Answered::Open, PromptKind::Open, false)
    );
    assert_eq!(opened.paths, vec![input.clone()]);
    demo.wait_state(&format!(
        "clicks=0 name=\"\" opened={:?}",
        vec![input.clone()]
    ));

    press(&mut session, "Save");
    let saved = session
        .answer_prompt(&PickAnswer::Paths(vec![target.clone()]), wait)
        .unwrap();
    assert_eq!(saved.answered, Answered::Save);
    assert_eq!(saved.prompt.directory.as_deref(), Some(Path::new("/tmp")));
    assert_eq!(saved.prompt.suggested_name.as_deref(), Some("demo.txt"));
    demo.wait_state(&format!(
        "clicks=0 name=\"\" opened={:?} saved={:?}",
        vec![input.clone()],
        target
    ));

    press(&mut session, "Folder");
    let picked = session
        .answer_prompt(&PickAnswer::Paths(vec![folder.clone()]), wait)
        .unwrap();
    assert_eq!(
        picked.paths,
        vec![folder.clone()],
        "the typed folder, not the dialog's current one"
    );
    demo.wait_state(&format!(
        "clicks=0 name=\"\" opened={:?} saved={:?}",
        vec![folder],
        target
    ));

    press(&mut session, "Open");
    let cancelled = session.answer_prompt(&PickAnswer::Cancel, wait).unwrap();
    assert_eq!(cancelled.answered, Answered::Cancelled);
    assert!(cancelled.paths.is_empty());

    press(&mut session, "Save");
    let two = session
        .answer_prompt(
            &PickAnswer::Paths(vec![home.join("a"), home.join("b")]),
            wait,
        )
        .unwrap_err();
    assert!(matches!(two, SessionError::InvalidArgument(_)), "{two}");
    let exists = session
        .answer_prompt(&PickAnswer::Paths(vec![input.clone()]), wait)
        .unwrap_err();
    assert!(
        matches!(exists, SessionError::InvalidArgument(_)),
        "overwriting is unsupported: {exists}"
    );
    let still = session.answer_prompt(&PickAnswer::Cancel, wait).unwrap();
    assert_eq!(
        still.answered,
        Answered::Cancelled,
        "the dialog stayed open for another answer"
    );

    press(&mut session, "Open");
    let missing = session
        .answer_prompt(&PickAnswer::Paths(vec![home.join("missing.txt")]), wait)
        .unwrap_err();
    assert!(
        matches!(missing, SessionError::InvalidArgument(_)),
        "{missing}"
    );
    let none = session
        .answer_prompt(&PickAnswer::Cancel, Duration::from_millis(500))
        .unwrap_err();
    assert!(
        matches!(none, SessionError::Timeout(_)),
        "the refused dialog was cancelled: {none}"
    );
    let after = demo.lines_for(Duration::from_millis(300));
    assert!(after.iter().all(|l| !l.contains("missing")), "{after:?}");
    session.check_isolation().unwrap();
    println!("dialog ok");
}

#[test]
fn the_clipboard_round_trips_and_pastes_what_the_keymap_lacks() {
    outer("inner_clipboard", "clipboard ok");
}

#[test]
#[ignore = "runs inside the sandbox through the_clipboard_round_trips_and_pastes_what_the_keymap_lacks"]
fn inner_clipboard() {
    if !inner() {
        return;
    }
    use gpui_pilot_protocol::{Node, PickAnswer};
    let mut session = Session::start(OUTPUT).unwrap();
    assert_eq!(
        session.clipboard(None).unwrap(),
        None,
        "a fresh session's clipboard is empty"
    );
    assert_eq!(
        session.clipboard(Some("héllo ✓ 漢")).unwrap().as_deref(),
        Some("héllo ✓ 漢")
    );
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes.iter().any(|n| n.id.as_deref() == Some("name"))
    });
    let centre = |pick: &dyn Fn(&Node) -> bool| {
        let b = &nodes.iter().find(|n| pick(n)).unwrap().bounds;
        (b.x + b.w / 2., b.y + b.h / 2.)
    };
    let button = |name: &'static str| {
        move |n: &Node| n.role.as_deref() == Some("Button") && n.name.as_deref() == Some(name)
    };
    let paste = centre(&button("Paste"));
    let open = centre(&button("Open"));
    let name = centre(&|n: &Node| n.id.as_deref() == Some("name"));

    session
        .click(demo.pid(), paste, MouseButton::Left, 1, &[])
        .unwrap();
    demo.wait_state("clicks=0 name=\"\" pasted=\"héllo ✓ 漢\"");
    session
        .click(demo.pid(), name, MouseButton::Left, 1, &[])
        .unwrap();
    demo.lines_for(Duration::from_millis(300));
    session.check_text("a漢b").unwrap();
    session.type_or_paste("a漢b").unwrap();
    demo.wait_state("clicks=0 name=\"a漢b\" pasted=\"héllo ✓ 漢\"");
    session.keys("ctrl-a ctrl-c").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        session.clipboard(None).unwrap().as_deref(),
        Some("a漢b"),
        "the app's copy is readable"
    );

    let kanji = std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("漢.txt");
    std::fs::write(&kanji, "k").unwrap();
    session
        .click(demo.pid(), open, MouseButton::Left, 1, &[])
        .unwrap();
    let opened = session
        .answer_prompt(
            &PickAnswer::Paths(vec![kanji.clone()]),
            Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(
        opened.paths,
        vec![kanji],
        "a path outside the keymap is pasted into the dialog"
    );
    println!("clipboard ok");
}

#[test]
fn eis_drags_the_chip_and_the_fallback_prompt_stays_out_of_the_tree() {
    outer("inner_drag_and_prompt", "drag ok");
}

#[test]
#[ignore = "runs inside the sandbox through eis_drags_the_chip_and_the_fallback_prompt_stays_out_of_the_tree"]
fn inner_drag_and_prompt() {
    if !inner() {
        return;
    }
    let mut session = Session::start(OUTPUT).unwrap();
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        ["chip", "dropzone", "ask", "count"]
            .iter()
            .all(|id| nodes.iter().any(|n| n.id.as_deref() == Some(*id)))
    });
    let centre = |id: &str| {
        let b = &nodes
            .iter()
            .find(|n| n.id.as_deref() == Some(id))
            .unwrap()
            .bounds;
        (b.x + b.w / 2., b.y + b.h / 2.)
    };

    // Released over a button without a drop handler: no drop, no click.
    session
        .drag(
            demo.pid(),
            centre("chip"),
            centre("count"),
            10,
            MouseButton::Left,
            &[],
        )
        .unwrap();
    let missed = demo.lines_for(Duration::from_millis(500));
    assert!(
        missed
            .iter()
            .all(|l| !l.contains("dropped=") && !l.contains("clicks=1")),
        "{missed:?}"
    );
    session
        .drag(
            demo.pid(),
            centre("chip"),
            centre("dropzone"),
            10,
            MouseButton::Left,
            &[],
        )
        .unwrap();
    demo.wait_state("clicks=0 name=\"\" dropped=1");

    // GPUI's fallback prompt is drawn into the window but adds nothing to the AT-SPI tree.
    // Its scrim covers the whole window, so a corner far from the Ask button changes too. The
    // unplaced window runs past the output, so the corner is that of its visible part.
    let client = session.window(demo.pid()).unwrap().client;
    let visible = |start: f32, len: f32, output: u32| (start + len).min(output as f32) - 4.;
    let corner = (
        (visible(client.x, client.w, OUTPUT.width) * 1.5) as u32,
        (visible(client.y, client.h, OUTPUT.height) * 1.5) as u32,
    );
    let before = session.capture().unwrap().pixel(corner.0, corner.1);
    session
        .click(demo.pid(), centre("ask"), MouseButton::Left, 1, &[])
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let after = session.capture().unwrap().pixel(corner.0, corner.1);
    assert_ne!(before, after, "the fallback prompt's scrim is drawn");
    let nodes = session.tree(demo.pid()).unwrap();
    for text in [
        "Delete",
        "Cancel",
        "Delete the chip?",
        "This cannot be undone.",
    ] {
        assert!(
            nodes
                .iter()
                .all(|n| n.name.as_deref() != Some(text) && n.value.as_deref() != Some(text)),
            "{text} is in the tree: {nodes:#?}"
        );
    }
    println!("drag ok");
}

/// The inner test's output, matching the one the outer test prepared the sandbox with.
fn refresh_output() -> Output {
    let scale = std::env::var("GP_TEST_SCALE").map_or(1.0, |s| s.parse().unwrap());
    Output::new(1280, 800, scale)
}

#[test]
fn kwin_advertises_the_custom_mode_it_was_given() {
    if !session_tests() {
        return;
    }
    for (scale, hz, want) in [
        (1.0, 144, 143_832),
        (1.5, 144, 143_838),
        (1.0, 240, 239_776),
        (1.0, 60, 60_000),
    ] {
        let output = Output::new(1280, 800, scale).with_refresh_hz(hz);
        let out = run_inside("inner_refresh", output, |s| {
            s.vars.push(("GP_TEST_SCALE".into(), scale.to_string()));
        });
        assert!(
            out.contains(&format!("refresh {want}\n")),
            "{scale} {hz}: {out}"
        );
    }
}

#[test]
#[ignore = "runs inside the sandbox through kwin_advertises_the_custom_mode_it_was_given"]
fn inner_refresh() {
    if !inner() {
        return;
    }
    let session = Session::start(refresh_output()).unwrap();
    println!("refresh {}", session.refresh_mhz().unwrap());
}

#[test]
fn a_mode_one_millihertz_off_falls_back_to_sixty() {
    if !session_tests() {
        return;
    }
    let out = run_inside(
        "inner_corrupted_mode",
        Output::new(1280, 800, 1.0).with_refresh_hz(144),
        |_| {},
    );
    assert!(out.contains("refresh 60000\n"), "{out}");
}

#[test]
#[ignore = "runs inside the sandbox through a_mode_one_millihertz_off_falls_back_to_sixty"]
fn inner_corrupted_mode() {
    if !inner() {
        return;
    }
    // The sandbox's private HOME holds the config KWin is about to read.
    let path =
        std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/kwinoutputconfig.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    // One clock step off the custom entry makes the mode match nothing KWin generated.
    assert_eq!(json[0]["data"][0]["mode"]["cvt"]["clock"], 177_500);
    json[0]["data"][0]["mode"]["cvt"]["clock"] = 177_750.into();
    std::fs::write(&path, json.to_string()).unwrap();
    let session = Session::start(refresh_output()).unwrap();
    println!("refresh {}", session.refresh_mhz().unwrap());
}

fn frames_output() -> Output {
    Output::new(1280, 800, 1.0).with_refresh_hz(144)
}

#[test]
fn frames_stamp_the_windows_damage_until_it_closes() {
    if !session_tests() {
        return;
    }
    let out = run_inside("inner_frames", frames_output(), |_| {});
    assert!(out.contains("frames ok"), "{out}");
}

#[test]
#[ignore = "runs inside the sandbox through frames_stamp_the_windows_damage_until_it_closes"]
fn inner_frames() {
    if !inner() {
        return;
    }
    let mut session = Session::start(frames_output()).unwrap();
    let refresh = session.refresh_mhz().unwrap();
    assert_eq!(refresh, 143_832);
    let early = session.frames_stop().unwrap_err();
    assert!(
        matches!(early, SessionError::InvalidArgument(_)),
        "{early:?}"
    );
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes.iter().any(|n| n.name.as_deref() == Some("Spin"))
    });
    let spin = nodes
        .iter()
        .find(|n| n.name.as_deref() == Some("Spin"))
        .unwrap()
        .bounds
        .clone();

    session.frames_start(demo.pid(), refresh, 100).unwrap();
    let twice = session.frames_start(demo.pid(), refresh, 100).unwrap_err();
    assert!(
        matches!(twice, SessionError::InvalidArgument(_)),
        "{twice:?}"
    );
    let at = (spin.x + spin.w / 2., spin.y + spin.h / 2.);
    session
        .click(demo.pid(), at, MouseButton::Left, 1, &[])
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let report = session.frames_stop().unwrap();
    assert_eq!(report.refresh_mhz, 143_832);
    // The spinner redraws every frame; even a dev build stays above 30 Hz.
    assert!(report.frames >= 30, "{report:?}");
    assert!(report.intervals.is_some(), "{report:?}");
    assert!(report.latency.is_none(), "no notches: {report:?}");
    assert!(
        report.work.is_none() && report.build.is_none(),
        "{report:?}"
    );
    assert!(!report.truncated, "{report:?}");

    // A recording ends, truncated, when its window closes.
    session.frames_start(demo.pid(), refresh, 100).unwrap();
    assert!(
        session
            .close_window(demo.pid(), Duration::from_secs(5))
            .unwrap()
    );
    // `FramesEnded` is its own zbus task, unordered against the geometry callback, so allow it
    // up to 500 ms to land.
    let deadline = Instant::now() + Duration::from_millis(500);
    while !session.frames_ended() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let ended = session.frames_stop().unwrap();
    assert!(ended.truncated, "{ended:?}");
    let gone = session.frames_start(demo.pid(), refresh, 100).unwrap_err();
    assert!(
        matches!(
            gone,
            SessionError::LaunchFailed {
                stage: "window",
                ..
            }
        ),
        "{gone:?}"
    );
    println!("frames ok");
}

#[test]
fn eis_sends_each_notch_as_one_wheel_event_with_modifiers_held() {
    outer("inner_notches", "notches ok");
}

#[test]
#[ignore = "runs inside the sandbox through eis_sends_each_notch_as_one_wheel_event_with_modifiers_held"]
fn inner_notches() {
    if !inner() {
        return;
    }
    let mut session = Session::start(OUTPUT).unwrap();
    let list = Demo::spawn_args(&["--list"]);
    list.wait_state("opened");
    session
        .wait_window(list.pid(), Duration::from_secs(15))
        .unwrap();
    let refresh = session.refresh_mhz().unwrap();
    session.frames_start(list.pid(), refresh, 100).unwrap();
    let started = Instant::now();
    session
        .notches(list.pid(), (300., 200.), &Wheel::new(0, 3, 5), &[])
        .unwrap();
    let took = started.elapsed();
    // The upper bound is loose on purpose: it includes a geometry script round trip on a debug
    // build; `pace`'s unit test pins the deadlines themselves.
    assert!(
        took >= Duration::from_millis(10) && took < Duration::from_secs(2),
        "notch i is due at i * 5 ms: {took:?}"
    );
    // Two notches merged into one event would skip straight past `wheels=2` to a wrong count.
    list.wait_state("clicks=0 name=\"\" wheels=3 lines=-9");
    std::thread::sleep(Duration::from_millis(100));
    let report = session.frames_stop().unwrap();
    assert_eq!(
        report.latency.as_ref().map(|l| l.count),
        Some(3),
        "{report:?}"
    );
    assert!(report.send_lag_max_ms.is_some(), "{report:?}");
    drop(list);

    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes.iter().any(|n| n.id.as_deref() == Some("name"))
    });
    let b = &nodes
        .iter()
        .find(|n| n.id.as_deref() == Some("name"))
        .unwrap()
        .bounds;
    let at = (b.x + b.w / 2., b.y + b.h / 2.);
    // The demo counts one zoom per wheel event with control held.
    session
        .notches(demo.pid(), at, &Wheel::new(0, 2, 16), &[Modifier::Ctrl])
        .unwrap();
    demo.wait_state("clicks=0 name=\"\" zoom=2");
    session
        .notches(demo.pid(), at, &Wheel::new(0, 1, 16), &[])
        .unwrap();
    demo.wait_state("clicks=0 name=\"\" scrolled zoom=2");
    println!("notches ok");
}

#[test]
fn many_pastes_after_keys_each_land_once() {
    outer("inner_paste_stress", "paste stress ok");
}

#[test]
#[ignore = "runs inside the sandbox through many_pastes_after_keys_each_land_once"]
fn inner_paste_stress() {
    if !inner() {
        return;
    }
    use gpui_pilot_protocol::Node;
    let mut session = Session::start(OUTPUT).unwrap();
    let demo = Demo::spawn();
    demo.wait_state("opened");
    session
        .wait_window(demo.pid(), Duration::from_secs(15))
        .unwrap();
    let nodes = settled_tree(&mut session, demo.pid(), |nodes| {
        nodes.iter().any(|n| n.id.as_deref() == Some("name"))
    });
    let name = {
        let b = &nodes
            .iter()
            .find(|n: &&Node| n.id.as_deref() == Some("name"))
            .unwrap()
            .bounds;
        (b.x + b.w / 2., b.y + b.h / 2.)
    };
    session
        .click(demo.pid(), name, MouseButton::Left, 1, &[])
        .unwrap();
    demo.lines_for(Duration::from_millis(300));
    // A key right before each paste is issue #3's "a漢" case; every pasted character differs,
    // so a stale paste shows as a repeated one.
    let text = "a漢b字c語d文e書f道g山h川i花j鳥";
    session.check_text(text).unwrap();
    session.type_or_paste(text).unwrap();
    demo.wait_state(&format!("clicks=0 name={text:?}"));
    println!("paste stress ok");
}
