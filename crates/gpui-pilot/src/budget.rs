//! Bounds the size of a `tree` reply on request: when the caller passes `max_chars` and the reply
//! is longer, runs of repeated sibling nodes leave `nodes` and are summarised in a top-level
//! `collapsed` array. Without `max_chars` a reply is never touched.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// Siblings sharing a shape form a run only when at least this many appear.
const MIN_RUN: usize = 4;

/// The most member ids a collapsed entry lists per state.
const MAX_FLAGGED: usize = 5;

/// Collapses repeated sibling runs when the reply's JSON is longer than `max_chars`. A run is
/// siblings with one parent, role and digit-normalised name; its members and their descendants
/// leave `nodes`, so every remaining entry keeps the node shape, and one entry in the top-level
/// `collapsed` array names the run. The reply then gains `shortened`. A reply within the limit
/// is untouched.
pub(crate) fn shorten_tree(reply: &mut Value, max_chars: usize) {
    let from_chars = reply.to_string().chars().count();
    if from_chars <= max_chars {
        return;
    }
    let Some(Value::Array(nodes)) = reply.get_mut("nodes") else {
        return;
    };
    let before = nodes.len();
    let (kept, runs) = collapse(std::mem::take(nodes), MIN_RUN);
    let changed = kept.len() != before;
    reply["nodes"] = Value::Array(kept);
    if changed {
        reply["collapsed"] = Value::Array(runs);
    }
    let hint = if changed {
        "Repeated sibling nodes were moved out of `nodes` into `collapsed` entries to keep this \
         reply small. Call tree again with `within` set to a run's `parent` to list its nodes."
    } else {
        "This reply is over `max_chars` and has no repeated sibling runs to collapse. \
         Call tree again with `within` set to the id of the part you need."
    };
    reply["shortened"] = serde_json::json!({
        "from_chars": from_chars, "max_chars": max_chars, "still_over": false, "hint": hint,
    });
    // Measured with the field in place, since the field itself adds to the size.
    let still_over = reply.to_string().chars().count() > max_chars;
    reply["shortened"]["still_over"] = still_over.into();
}

fn collapse(nodes: Vec<Value>, min_run: usize) -> (Vec<Value>, Vec<Value>) {
    // Group sibling nodes by shape, remembering each group's members in reply order.
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if let Some(key) = shape(node) {
            groups.entry(key).or_default().push(index);
        }
    }
    // Shallow runs first, so a run nested in an already collapsed one is dropped, not summarised.
    let mut runs: Vec<Vec<usize>> = groups
        .into_values()
        .filter(|members| members.len() >= min_run)
        .collect();
    runs.sort_by_key(|members| (path_of(&nodes[members[0]]).len(), members[0]));

    let mut gone: HashSet<usize> = HashSet::new();
    let mut summaries: Vec<(usize, Value)> = Vec::new();
    let mut roots: HashSet<String> = HashSet::new();
    for members in runs {
        let live: Vec<usize> = members
            .into_iter()
            .filter(|&i| !under_root(path_of(&nodes[i]), &roots))
            .collect();
        if live.len() < min_run {
            continue;
        }
        let member_paths: HashSet<String> =
            live.iter().map(|&i| path_key(path_of(&nodes[i]))).collect();
        let mut dropped = 0;
        for (i, node) in nodes.iter().enumerate() {
            let path = path_of(node);
            let hit = (1..=path.len()).any(|n| member_paths.contains(&path_key(&path[..n])));
            if hit && gone.insert(i) {
                dropped += 1;
            }
        }
        let (first, last) = (&nodes[live[0]], &nodes[live[live.len() - 1]]);
        let state = |key: &str| -> Vec<Value> {
            live.iter()
                .map(|&i| &nodes[i])
                .filter(|n| n[key] == Value::Bool(true))
                .take(MAX_FLAGGED)
                .map(label)
                .collect()
        };
        let parent = &path_of(first)[..path_of(first).len().saturating_sub(1)];
        let mut run = serde_json::json!({"count": live.len(), "first": label(first),
            "last": label(last), "nodes": dropped, "role": first["role"],
            "parent": parent});
        for key in ["focused", "checked", "selected", "expanded"] {
            let members = state(key);
            if !members.is_empty() {
                run[key] = members.into();
            }
        }
        summaries.push((live[0], run));
        roots.extend(member_paths);
    }
    // Summaries keep the reply order of their first member.
    summaries.sort_by_key(|(first, _)| *first);
    let kept = nodes
        .into_iter()
        .enumerate()
        .filter_map(|(i, node)| (!gone.contains(&i)).then_some(node))
        .collect();
    (kept, summaries.into_iter().map(|(_, run)| run).collect())
}

fn path_of(node: &Value) -> &[Value] {
    node["path"].as_array().map_or(&[], Vec::as_slice)
}

fn path_key(path: &[Value]) -> String {
    Value::Array(path.to_vec()).to_string()
}

fn under_root(path: &[Value], roots: &HashSet<String>) -> bool {
    (1..=path.len()).any(|n| roots.contains(&path_key(&path[..n])))
}

/// What a collapsed run is called: its member's id, else its last path segment.
fn label(node: &Value) -> Value {
    match node["id"].as_str() {
        Some(id) => id.into(),
        None => path_of(node).last().cloned().unwrap_or(Value::Null),
    }
}

/// The grouping key: parent path, role and the last segment with digit runs folded to `#`.
fn shape(node: &Value) -> Option<String> {
    let path = path_of(node);
    let last = path.last()?;
    let name = match last {
        Value::String(text) => fold_digits(text),
        _ => "#".to_owned(),
    };
    let role = node["role"].as_str().unwrap_or("");
    Some(format!(
        "{}|{role}|{name}",
        path_key(&path[..path.len() - 1])
    ))
}

fn fold_digits(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if !c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('#') {
            out.push('#');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MAX: usize = 30_000;

    fn row(i: usize) -> Value {
        json!({"path": ["window", "list", format!("body-line-{i}")], "id": format!("body-line-{i}"),
               "role": "ListItem", "name": format!("line number {i} of the response body"),
               "bounds": {"x": 0, "y": i * 20, "w": 400, "h": 20}, "visible": true, "depth": 2})
    }

    fn rows(count: usize) -> Value {
        let mut nodes = vec![json!({"path": ["window"], "id": "window", "role": "Window",
            "bounds": {"x": 0, "y": 0, "w": 800, "h": 600}, "visible": true, "depth": 0})];
        nodes.extend((0..count).map(row));
        json!({"nodes": nodes})
    }

    fn chars(reply: &Value) -> usize {
        reply.to_string().chars().count()
    }

    #[test]
    fn thousands_of_sibling_rows_collapse_into_the_collapsed_array_under_the_limit() {
        let mut reply = rows(4000);
        shorten_tree(&mut reply, MAX);
        assert!(chars(&reply) < MAX, "{}", chars(&reply));
        let nodes = reply["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1, "only the window stays in nodes");
        assert!(nodes[0].get("collapsed").is_none());
        let runs = reply["collapsed"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["count"], json!(4000));
        assert_eq!(runs[0]["first"], json!("body-line-0"));
        assert_eq!(runs[0]["last"], json!("body-line-3999"));
        assert_eq!(runs[0]["parent"], json!(["window", "list"]));
        assert_eq!(reply["shortened"]["max_chars"], json!(MAX));
        let hint = reply["shortened"]["hint"].as_str().unwrap();
        assert!(hint.contains("within"));
    }

    #[test]
    fn every_node_left_in_nodes_keeps_the_node_shape() {
        let mut reply = rows(400);
        reply["nodes"].as_array_mut().unwrap().push(
            json!({"path": ["window", "footer"], "id": "footer", "role": "Group",
            "bounds": {"x": 0, "y": 580, "w": 800, "h": 20}, "visible": true, "depth": 1}),
        );
        shorten_tree(&mut reply, MAX);
        for node in reply["nodes"].as_array().unwrap() {
            assert!(node["path"].as_array().is_some(), "{node}");
            assert!(node.get("collapsed").is_none());
        }
    }

    #[test]
    fn a_reply_within_the_limit_is_untouched() {
        let mut reply = rows(40);
        let before = reply.to_string();
        shorten_tree(&mut reply, MAX);
        assert_eq!(reply.to_string(), before);
        assert!(reply.get("collapsed").is_none() && reply.get("shortened").is_none());
    }

    #[test]
    fn descendants_of_collapsed_rows_go_with_them_and_other_nodes_stay() {
        let mut reply = rows(400);
        let nodes = reply["nodes"].as_array_mut().unwrap();
        nodes.push(
            json!({"path": ["window", "list", "body-line-7", "cell"], "role": "Text",
            "bounds": {"x": 0, "y": 0, "w": 1, "h": 1}, "visible": true, "depth": 3}),
        );
        nodes.push(
            json!({"path": ["window", "footer"], "id": "footer", "role": "Group",
            "bounds": {"x": 0, "y": 580, "w": 800, "h": 20}, "visible": true, "depth": 1}),
        );
        shorten_tree(&mut reply, MAX);
        let nodes = reply["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1]["id"], json!("footer"));
        assert_eq!(reply["collapsed"][0]["nodes"], json!(401));
    }

    #[test]
    fn a_few_distinct_siblings_are_never_collapsed() {
        let mut reply = rows(0);
        let nodes = reply["nodes"].as_array_mut().unwrap();
        for name in ["save", "open", "close"] {
            nodes.push(
                json!({"path": ["window", name], "id": name, "role": "Button",
                "name": "x".repeat(10_000),
                "bounds": {"x": 0, "y": 0, "w": 1, "h": 1}, "visible": true, "depth": 1}),
            );
        }
        shorten_tree(&mut reply, MAX);
        assert_eq!(reply["nodes"].as_array().unwrap().len(), 4);
        assert!(reply.get("collapsed").is_none());
        assert!(reply["shortened"]["still_over"].as_bool().unwrap());
    }

    #[test]
    fn a_tree_without_runs_stays_whole_and_says_it_is_still_over() {
        let nodes: Vec<Value> = (0..180)
            .map(|i| {
                let path: Vec<String> = (0..12)
                    .map(|d| format!("{}-{i}-{d}", "segment".repeat(7)))
                    .collect();
                json!({"path": path, "id": format!("node-{i}"), "role": format!("Role{}", i % 7),
                       "bounds": {"x": 0, "y": i, "w": 9, "h": 1}, "visible": true, "depth": 12})
            })
            .collect();
        let mut reply = json!({ "nodes": nodes });
        shorten_tree(&mut reply, MAX);
        assert_eq!(reply["nodes"].as_array().unwrap().len(), 180);
        assert_eq!(reply["shortened"]["still_over"], json!(true));
        let hint = reply["shortened"]["hint"].as_str().unwrap();
        assert!(
            hint.contains("within") && !hint.contains("moved out"),
            "{hint}"
        );
    }

    #[test]
    fn a_collapsed_entry_keeps_member_state() {
        let mut reply = rows(400);
        reply["nodes"][3]["focused"] = json!(true);
        reply["nodes"][5]["selected"] = json!(true);
        shorten_tree(&mut reply, MAX);
        let entry = &reply["collapsed"][0];
        assert_eq!(entry["focused"], json!(["body-line-2"]));
        assert_eq!(entry["selected"], json!(["body-line-4"]));
        assert_eq!(entry["role"], json!("ListItem"));
    }

    #[test]
    fn two_or_three_similar_siblings_are_not_a_run() {
        let mut reply = rows(3);
        reply["nodes"][1]["name"] = json!("x".repeat(30_000));
        shorten_tree(&mut reply, MAX);
        assert_eq!(reply["nodes"].as_array().unwrap().len(), 4);
    }
}
