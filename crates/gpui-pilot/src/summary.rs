//! The text block of every tool result and error: the structured content's key facts as lines.

use gpui_pilot_protocol::PilotError;
use serde_json::Value;

/// Clipboard text past this many characters is cut in the text block.
const CLIPBOARD_CHARS: usize = 500;
/// An error line names at most this many matches.
const ERROR_MATCHES: usize = 5;

/// The text that accompanies a successful tool's structured content `value`: one line per fact
/// group, plus one line per node for `tree`. Unknown tools report `done`.
pub(crate) fn summary(tool: &str, value: &Value) -> String {
    let body = match tool {
        "click" => targeted("clicked", &value["clicked"], &value["hit"]),
        "hover" => targeted("hovered", &value["hovered"], &value["hit"]),
        "drag" => format!(
            "dragged {} to {}",
            describe(&value["from"]),
            describe(&value["to"])
        ),
        "drop" => {
            let verb = if value["dropped"] == Value::Bool(false) {
                "dragged files over and away from"
            } else {
                "dropped files on"
            };
            targeted(verb, &value["over"], &value["hit"])
        }
        "resize" => {
            let window = &value["window"];
            format!(
                "window is {}x{} at scale {}",
                number(&window["w"]),
                number(&window["h"]),
                number(&window["scale"])
            )
        }
        "dialog" => dialog_line(value),
        "clipboard" => clipboard(&value["text"]),
        "launch" => {
            let mut line = format!(
                "{} app running as pid {}",
                value["mode"].as_str().unwrap_or("?"),
                number(&value["pid"])
            );
            if let Some(network) = value["network"].as_str() {
                line.push_str(&format!(" on network {network}"));
            }
            if let Some(hz) = value["refresh_hz"].as_f64() {
                line.push_str(&format!(" at {hz:.3} Hz"));
            }
            line
        }
        "close" => {
            // A refused window close is the whole story; its `settled` adds nothing to act on.
            if value["closed"] == Value::Bool(false) {
                return "close: the app kept its window open".to_owned();
            }
            let mut line = match value.get("exit_code").filter(|code| !code.is_null()) {
                Some(code) => format!("exited with code {}", number(code)),
                None => "exited without a code".to_owned(),
            };
            if let Some(dir) = value["data_dir"].as_str() {
                line.push_str(&format!("; data kept in {dir}"));
            }
            if value["closed"] == Value::Bool(true) {
                line.insert_str(0, "window closed; ");
            }
            line
        }
        "wait" => match value.get("node").filter(|node| !node.is_null()) {
            Some(node) => format!(
                "found {} after {} ms",
                describe(node),
                number(&value["waited_ms"])
            ),
            None => format!("waited {} ms", number(&value["waited_ms"])),
        },
        "pick_file" => pick_file(value),
        "screenshot" => {
            let mut line = format!(
                "{}x{} at scale {}",
                number(&value["w"]),
                number(&value["h"]),
                number(&value["scale"])
            );
            if let Some(saved) = value["saved_to"].as_str() {
                line.push_str(&format!("; saved to {saved}"));
            }
            line
        }
        "type" => match value["redacted"].as_str() {
            Some(placeholder) => format!("typed {placeholder}"),
            None => "done".to_owned(),
        },
        "tree" => return tree(value),
        "frames" => frames(value),
        _ => "done".to_owned(),
    };
    format!("{tool}: {body}{}", settle_tail(value))
}

/// One line for a failed call: `<Code>: <message>` plus its hit, matches and nearest ids.
pub(crate) fn error_line(error: &PilotError) -> String {
    let mut line = format!("{:?}: {}", error.code, error.message);
    if let Some(hit) = &error.hit {
        line.push_str(&format!("; hit {}", describe(&to_value(hit))));
    }
    if !error.matches.is_empty() {
        let named: Vec<String> = error
            .matches
            .iter()
            .take(ERROR_MATCHES)
            .map(|node| describe(&to_value(node)))
            .collect();
        line.push_str(&format!("; matches: {}", named.join(", ")));
        if error.matches.len() > ERROR_MATCHES {
            line.push_str(&format!(" (+{} more)", error.matches.len() - ERROR_MATCHES));
        }
    }
    if !error.nearest.is_empty() {
        let named: Vec<String> = error
            .nearest
            .iter()
            .map(|path| last_segment(&to_value(path)))
            .collect();
        line.push_str(&format!("; nearest: {}", named.join(", ")));
    }
    // Messages come from many places; a stray newline must not break the one-line contract.
    line.replace('\n', " ")
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// A JSON number printed through `f64`, so `16.0` reads `16`; anything else prints as JSON.
fn number(value: &Value) -> String {
    value
        .as_f64()
        .map_or_else(|| value.to_string(), |n| n.to_string())
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

/// The settle state appended to an action's line; tools without one get nothing.
fn settle_tail(value: &Value) -> &'static str {
    match value["settled"].as_bool() {
        Some(true) => "; settled",
        Some(false) => "; not settled (settle_ms ran out)",
        None => "",
    }
}

/// A path's own id: its last segment as JSON in brackets, or `(no element)` for an empty path.
fn last_segment(path: &Value) -> String {
    match path.as_array().and_then(|segments| segments.last()) {
        Some(last) => format!("[{last}]"),
        None => "(no element)".to_owned(),
    }
}

/// A node as `["id"] Role "Name"`, with the role and name only when present. A session node's
/// accessible `id` wins over its path, whose segments there are only child indices.
fn describe(node: &Value) -> String {
    let mut text = match node["id"].as_str() {
        Some(id) => format!("[{}]", quoted(id)),
        None => last_segment(&node["path"]),
    };
    if let Some(role) = node["role"].as_str() {
        text.push_str(&format!(" {role}"));
    }
    if let Some(name) = node["name"].as_str() {
        text.push_str(&format!(" {}", quoted(name)));
    }
    text
}

/// The click and hover line: the target, then where the pointer actually landed.
fn targeted(verb: &str, target: &Value, hit: &Value) -> String {
    let hit = if hit == target {
        "the same element".to_owned()
    } else {
        describe(hit)
    };
    format!("{verb} {}; hit {hit}", describe(target))
}

/// A dialog as `level "message" with buttons "A", "B"`.
fn dialog_text(dialog: &Value) -> String {
    let buttons: Vec<String> = dialog["buttons"]
        .as_array()
        .map(|b| b.iter().filter_map(Value::as_str).map(quoted).collect())
        .unwrap_or_default();
    format!(
        "{} {} with buttons {}",
        dialog["level"].as_str().unwrap_or("info"),
        quoted(dialog["message"].as_str().unwrap_or("")),
        buttons.join(", ")
    )
}

/// The `dialog` line: the open dialog, what answered it, or that none is open.
fn dialog_line(value: &Value) -> String {
    let dialog = value.get("dialog").filter(|d| !d.is_null());
    match (value["answered"].as_str(), dialog) {
        (Some(button), Some(dialog)) => {
            format!("answered {} to {}", quoted(button), dialog_text(dialog))
        }
        (Some(button), None) => format!("answered {}", quoted(button)),
        (None, Some(dialog)) => dialog_text(dialog),
        (None, None) => "no dialog is open".to_owned(),
    }
}

fn clipboard(text: &Value) -> String {
    let Some(text) = text.as_str() else {
        return "empty".to_owned();
    };
    let total = text.chars().count();
    if total <= CLIPBOARD_CHARS {
        return quoted(text);
    }
    let kept: String = text.chars().take(CLIPBOARD_CHARS).collect();
    format!(
        "{} (+{} more characters)",
        quoted(&kept),
        total - CLIPBOARD_CHARS
    )
}

fn pick_file(value: &Value) -> String {
    let kind = value["prompt"]["kind"].as_str().unwrap_or("file");
    let mut line = match value["answered"].as_str() {
        Some("queued") => "queued for the next dialog".to_owned(),
        Some("cancelled") => format!("cancelled the {kind} dialog"),
        _ => format!("answered the {kind} dialog"),
    };
    if let Some(paths) = value.get("paths").filter(|paths| !paths.is_null()) {
        line.push_str(&format!(" with {paths}"));
    }
    line
}

/// A started recording names its rate; a stopped one its frames, intervals, budget overruns,
/// late notches and warnings.
fn frames(value: &Value) -> String {
    let hz = value["refresh_mhz"].as_f64().unwrap_or(0.0) / 1000.0;
    if value.get("frames").is_none() {
        return format!("recording at {hz:.3} Hz");
    }
    let mut line = format!(
        "{} frames in {} ms at {hz:.3} Hz; ",
        number(&value["frames"]),
        number(&value["duration_ms"])
    );
    let intervals = &value["intervals"];
    if intervals.is_object() {
        line.push_str(&format!(
            "interval median {} ms, p95 {} ms, max {} ms",
            number(&intervals["median_ms"]),
            number(&intervals["p95_ms"]),
            number(&intervals["max_ms"])
        ));
    } else {
        line.push_str("no intervals");
    }
    line.push_str(&format!(
        "; over budget {} of {} counted; late {}",
        number(&value["over_budget"]),
        number(&value["counted_intervals"]),
        number(&value["late"])
    ));
    let warnings: Vec<&str> = value["warnings"]
        .as_array()
        .map(|w| w.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !warnings.is_empty() {
        line.push_str(&format!("; warnings: {}", warnings.join(", ")));
    }
    line
}

/// The tree's header line with its dialog state, then one line per node with its flags.
fn tree(value: &Value) -> String {
    let nodes = value["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut header = format!("tree: {} nodes", nodes.len());
    if let Some(hint) = value["shortened"]["hint"].as_str() {
        header.push_str(&format!("; shortened: {hint}"));
    }
    if let Some(kind) = value["prompt"]["kind"].as_str() {
        let article = if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
            "an"
        } else {
            "a"
        };
        header.push_str(&format!(
            "; {article} {kind} dialog is waiting for pick_file"
        ));
    }
    if value["pick_queued"] == Value::Bool(true) {
        header.push_str("; a pick_file answer is queued for the next dialog");
    }
    if let Some(dialog) = value.get("dialog").filter(|d| !d.is_null()) {
        header.push_str(&format!(
            "; dialog {} is waiting for the dialog tool",
            dialog_text(dialog)
        ));
    }
    let mut lines = vec![header];
    if value.get("shortened").is_some() {
        // Collapsed runs first, then nodes while the text stays short; the structured content
        // holds the same runs under `collapsed` and the remaining nodes under `nodes`.
        let runs: &[Value] = value["collapsed"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        lines.extend(runs.iter().map(run_line));
        let mut used: usize = lines.iter().map(String::len).sum();
        let mut shown = 0;
        for node in nodes {
            let line = node_line(node);
            if used + line.len() > TEXT_BUDGET_CHARS {
                break;
            }
            used += line.len();
            lines.push(line);
            shown += 1;
        }
        if shown < nodes.len() {
            lines.push(format!(
                "- {} more nodes in the structured content",
                nodes.len() - shown
            ));
        }
    } else {
        lines.extend(nodes.iter().map(node_line));
    }
    lines.join("\n")
}

/// The longest node listing in a shortened tree's text, in characters.
const TEXT_BUDGET_CHARS: usize = 3000;

/// The states some run members have, as `; focused "a"; selected "b", "c"`.
fn flagged(run: &Value) -> String {
    let mut text = String::new();
    for key in ["focused", "checked", "selected", "expanded"] {
        if let Some(ids) = run[key].as_array() {
            let ids: Vec<String> = ids.iter().map(|i| quoted(&label_text(i))).collect();
            text.push_str(&format!("; {key} {}", ids.join(", ")));
        }
    }
    text
}

fn label_text(label: &Value) -> String {
    label
        .as_str()
        .map_or_else(|| label.to_string(), str::to_owned)
}

/// One `collapsed` entry of a shortened tree.
fn run_line(run: &Value) -> String {
    format!(
        "- {} similar {} nodes {} to {} ({} with descendants), collapsed{}",
        number(&run["count"]),
        run["role"].as_str().unwrap_or("-"),
        quoted(&label_text(&run["first"])),
        quoted(&label_text(&run["last"])),
        number(&run["nodes"]),
        flagged(run)
    )
}

fn node_line(node: &Value) -> String {
    let mut line = format!("- {}", describe(node));
    if let Some(value) = node["value"].as_str() {
        line.push_str(&format!(" value {}", quoted(value)));
    }
    let bounds = &node["bounds"];
    line.push_str(&format!(
        " at {},{} {}x{}",
        number(&bounds["x"]),
        number(&bounds["y"]),
        number(&bounds["w"]),
        number(&bounds["h"])
    ));
    // Flags in a fixed order; a tri-state attribute names both of its observed states.
    let flag = |key: &str| node[key].as_bool();
    let flags = [
        (flag("visible") == Some(false)).then_some("hidden"),
        (flag("disabled") == Some(true)).then_some("disabled"),
        flag("checked").map(|on| if on { "checked" } else { "unchecked" }),
        (flag("selected") == Some(true)).then_some("selected"),
        flag("expanded").map(|on| if on { "expanded" } else { "collapsed" }),
        (flag("focused") == Some(true)).then_some("focused"),
    ];
    for name in flags.into_iter().flatten() {
        line.push(' ');
        line.push_str(name);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::{ErrorCode, Node, PilotError, Rect, Segment};
    use serde_json::json;

    fn node(last: &str, role: Option<&str>, name: Option<&str>) -> Value {
        let mut node = json!({"path": ["root", last], "visible": true, "depth": 1,
            "bounds": {"x": 16.0, "y": 50.5, "w": 80.0, "h": 32.0}});
        if let Some(role) = role {
            node["role"] = json!(role);
        }
        if let Some(name) = name {
            node["name"] = json!(name);
        }
        node
    }

    #[test]
    fn a_resize_names_the_size_the_window_has() {
        let resized = json!({"window": {"w": 800.0, "h": 600.0, "scale": 1.5}, "settled": true});
        assert_eq!(
            summary("resize", &resized),
            "resize: window is 800x600 at scale 1.5; settled"
        );
    }

    #[test]
    fn a_drop_says_whether_it_dropped_or_left() {
        let files = node("files", Some("Button"), Some("Drop files"));
        let dropped = json!({"over": files, "hit": files, "dropped": true, "settled": true});
        assert_eq!(
            summary("drop", &dropped),
            r#"drop: dropped files on ["files"] Button "Drop files"; hit the same element; settled"#
        );
        let left = json!({"over": files, "hit": files, "dropped": false, "settled": true});
        assert_eq!(
            summary("drop", &left),
            r#"drop: dragged files over and away from ["files"] Button "Drop files"; hit the same element; settled"#
        );
    }

    #[test]
    fn click_and_hover_name_the_target_the_hit_and_the_settle() {
        let count = node("count", Some("Button"), Some("Count"));
        let same = json!({"clicked": count, "hit": count, "settled": true});
        assert_eq!(
            summary("click", &same),
            r#"click: clicked ["count"] Button "Count"; hit the same element; settled"#
        );
        let other =
            json!({"hovered": count, "hit": node("cover", Some("Dialog"), None), "settled": false});
        assert_eq!(
            summary("hover", &other),
            r#"hover: hovered ["count"] Button "Count"; hit ["cover"] Dialog; not settled (settle_ms ran out)"#
        );
    }

    #[test]
    fn a_session_node_shows_its_accessible_id_not_its_child_index() {
        // Session paths are AT-SPI child indices; only `id` gives the agent a target to reuse.
        let mut again = json!({"path": [0, 3], "id": "re-count", "role": "Button",
            "name": "Again", "visible": true, "depth": 1,
            "bounds": {"x": 16.0, "y": 50.0, "w": 80.0, "h": 32.0}});
        assert_eq!(
            summary(
                "click",
                &json!({"clicked": again, "hit": again, "settled": true})
            ),
            r#"click: clicked ["re-count"] Button "Again"; hit the same element; settled"#
        );
        again["id"] = Value::Null;
        assert_eq!(
            summary("wait", &json!({"node": again, "waited_ms": 0})),
            r#"wait: found [3] Button "Again" after 0 ms"#
        );
    }

    #[test]
    fn clipboard_text_is_quoted_and_long_text_is_cut() {
        assert_eq!(
            summary("clipboard", &json!({"text": "a \"b\"\n", "settled": true})),
            r#"clipboard: "a \"b\"\n"; settled"#
        );
        assert_eq!(
            summary("clipboard", &json!({"settled": true})),
            "clipboard: empty; settled"
        );
        let line = summary(
            "clipboard",
            &json!({"text": "x".repeat(600), "settled": true}),
        );
        assert!(
            line.starts_with(&format!("clipboard: \"{}\"", "x".repeat(500))),
            "{line}"
        );
        assert!(
            line.ends_with("\" (+100 more characters); settled"),
            "{line}"
        );
    }

    #[test]
    fn plain_actions_report_the_settle_and_the_rest_their_facts() {
        assert_eq!(
            summary("type", &json!({"settled": true})),
            "type: done; settled"
        );
        assert_eq!(
            summary(
                "type",
                &json!({"settled": true, "redacted": "<redacted 7 chars>"})
            ),
            "type: typed <redacted 7 chars>; settled"
        );
        assert_eq!(
            summary("key", &json!({"settled": false})),
            "key: done; not settled (settle_ms ran out)"
        );
        let launched = json!({"mode": "headless", "pid": 42, "network": "host", "settled": true});
        assert_eq!(
            summary("launch", &launched),
            "launch: headless app running as pid 42 on network host; settled"
        );
        let at_rate = json!({"mode": "session", "pid": 7, "network": "none", "refresh_hz": 143.832, "settled": true});
        assert_eq!(
            summary("launch", &at_rate),
            "launch: session app running as pid 7 on network none at 143.832 Hz; settled"
        );
        let whole = json!({"mode": "headless", "pid": 7, "network": "host", "refresh_hz": 144.0, "settled": true});
        assert_eq!(
            summary("launch", &whole),
            "launch: headless app running as pid 7 on network host at 144.000 Hz; settled"
        );
        assert_eq!(
            summary("close", &json!({"exit_code": 0, "log_tail": ""})),
            "close: exited with code 0"
        );
        assert_eq!(
            summary("close", &json!({"log_tail": ""})),
            "close: exited without a code"
        );
        assert_eq!(
            summary(
                "close",
                &json!({"exit_code": 0, "log_tail": "", "data_dir": "/t/gp-x/data"})
            ),
            "close: exited with code 0; data kept in /t/gp-x/data"
        );
        assert_eq!(
            summary("close", &json!({"closed": false, "settled": true})),
            "close: the app kept its window open"
        );
        assert_eq!(
            summary(
                "close",
                &json!({"closed": true, "exit_code": 0, "log_tail": ""})
            ),
            "close: window closed; exited with code 0"
        );
        assert_eq!(
            summary("wait", &json!({"waited_ms": 250})),
            "wait: waited 250 ms"
        );
        let late = json!({"node": node("late", Some("Button"), Some("Late")), "waited_ms": 40});
        assert_eq!(
            summary("wait", &late),
            r#"wait: found ["late"] Button "Late" after 40 ms"#
        );
        assert_eq!(
            summary("pick_file", &json!({"answered": "queued", "settled": true})),
            "pick_file: queued for the next dialog; settled"
        );
        let open = json!({"answered": "open", "prompt": {"kind": "open", "multiple": false}, "settled": true});
        assert_eq!(
            summary("pick_file", &open),
            "pick_file: answered the open dialog; settled"
        );
        let cancelled = json!({"answered": "cancelled", "prompt": {"kind": "save", "multiple": false}, "settled": true});
        assert_eq!(
            summary("pick_file", &cancelled),
            "pick_file: cancelled the save dialog; settled"
        );
        let staged = json!({"answered": "open", "prompt": {"kind": "open", "multiple": false},
            "paths": ["/in/1/a.yml"], "settled": true});
        assert_eq!(
            summary("pick_file", &staged),
            r#"pick_file: answered the open dialog with ["/in/1/a.yml"]; settled"#
        );
        let shot = json!({"w": 640, "h": 400, "scale": 1.0, "saved_to": "/tmp/a.png"});
        assert_eq!(
            summary("screenshot", &shot),
            "screenshot: 640x400 at scale 1; saved to /tmp/a.png"
        );
    }

    #[test]
    fn a_tree_lists_one_line_per_node_after_its_dialog_state() {
        let mut input = node("name", Some("TextInput"), None);
        input["value"] = json!("hi");
        input["disabled"] = json!(true);
        let mut hidden = node("late", None, None);
        hidden["visible"] = json!(false);
        let tree = json!({"nodes": [node("count", Some("Button"), Some("Count")), input, hidden],
            "prompt": {"kind": "open", "multiple": false}, "pick_queued": true});
        assert_eq!(summary("tree", &tree), [
            "tree: 3 nodes; an open dialog is waiting for pick_file; a pick_file answer is queued for the next dialog",
            r#"- ["count"] Button "Count" at 16,50.5 80x32"#,
            r#"- ["name"] TextInput value "hi" at 16,50.5 80x32 disabled"#,
            r#"- ["late"] at 16,50.5 80x32 hidden"#,
        ].join("\n"));
        assert_eq!(summary("tree", &json!({"nodes": []})), "tree: 0 nodes");
    }

    #[test]
    fn an_error_line_carries_the_hit_matches_and_nearest_on_one_line() {
        let cover = Node::new(
            vec![Segment::Str("cover".into())],
            Rect::new(0., 0., 9., 9.),
            true,
            0,
        );
        let mut occluded = PilotError::new(ErrorCode::Occluded, "covered");
        occluded.hit = Some(cover.clone());
        assert_eq!(error_line(&occluded), r#"Occluded: covered; hit ["cover"]"#);
        let mut ambiguous = PilotError::new(ErrorCode::Ambiguous, "7 elements match");
        ambiguous.matches = vec![cover; 7];
        let line = error_line(&ambiguous);
        assert!(
            line.starts_with(r#"Ambiguous: 7 elements match; matches: ["cover"], ["cover"]"#),
            "{line}"
        );
        assert!(
            line.ends_with(" (+2 more)") && !line.contains('\n'),
            "{line}"
        );
        let mut missing = PilotError::new(ErrorCode::ElementNotFound, "no element");
        missing.nearest = vec![vec![
            Segment::Str("a".into()),
            Segment::Str("0:json.yml".into()),
        ]];
        assert_eq!(
            error_line(&missing),
            r#"ElementNotFound: no element; nearest: ["0:json.yml"]"#
        );
    }

    #[test]
    fn drag_and_dialog_lines_name_their_facts() {
        let dragged = json!({"from": node("chip", Some("Button"), Some("Chip")),
            "to": node("dropzone", Some("Button"), Some("Drop here")), "settled": true});
        assert_eq!(
            summary("drag", &dragged),
            r#"drag: dragged ["chip"] Button "Chip" to ["dropzone"] Button "Drop here"; settled"#
        );
        let shown = json!({"level": "warning", "message": "Delete the chip?",
            "buttons": ["Delete", "Cancel"]});
        assert_eq!(
            summary("dialog", &json!({"settled": true})),
            "dialog: no dialog is open; settled"
        );
        assert_eq!(
            summary("dialog", &json!({"dialog": shown, "settled": true})),
            r#"dialog: warning "Delete the chip?" with buttons "Delete", "Cancel"; settled"#
        );
        assert_eq!(
            summary(
                "dialog",
                &json!({"dialog": shown, "answered": "Cancel", "settled": false})
            ),
            r#"dialog: answered "Cancel" to warning "Delete the chip?" with buttons "Delete", "Cancel"; not settled (settle_ms ran out)"#
        );
        let tree = json!({"nodes": [], "dialog": shown});
        assert_eq!(
            summary("tree", &tree),
            r#"tree: 0 nodes; dialog warning "Delete the chip?" with buttons "Delete", "Cancel" is waiting for the dialog tool"#
        );
    }

    #[test]
    fn frames_lines_name_the_rate_and_the_numbers_that_matter() {
        assert_eq!(
            summary("frames", &json!({"refresh_mhz": 143_832})),
            "frames: recording at 143.832 Hz"
        );
        let stopped = json!({"refresh_mhz": 60_000, "frames": 120, "duration_ms": 2040.5,
            "intervals": {"count": 119, "median_ms": 16.667, "p95_ms": 17.0, "p99_ms": 33.3, "max_ms": 33.4},
            "counted_intervals": 110, "over_budget": 1, "late": 0, "warnings": ["unoptimised_host"]});
        assert_eq!(
            summary("frames", &stopped),
            "frames: 120 frames in 2040.5 ms at 60.000 Hz; interval median 16.667 ms, p95 17 ms, \
             max 33.4 ms; over budget 1 of 110 counted; late 0; warnings: unoptimised_host"
        );
        let idle = json!({"refresh_mhz": 144_000, "frames": 0, "duration_ms": 300.0,
            "counted_intervals": 0, "over_budget": 0, "late": 0, "warnings": []});
        assert_eq!(
            summary("frames", &idle),
            "frames: 0 frames in 300 ms at 144.000 Hz; no intervals; over budget 0 of 0 counted; late 0"
        );
    }
}
