//! Keeps element text values in tool results bounded: a long `value` is cut to its head and
//! tail with its length beside it, and `tree` can ask for a slice of one element's value.

use serde_json::Value;

/// Values longer than this many characters are cut.
pub(crate) const VALUE_CAP_CHARS: usize = 2000;

/// The characters kept from each end of a cut value.
pub(crate) const VALUE_KEEP_CHARS: usize = 400;

/// The longest slice `tree`'s `value_slice` may ask for, in characters.
pub(crate) const MAX_SLICE_CHARS: u64 = 65_536;

/// Cuts every node value in `reply` longer than the cap. A node is any object with an array
/// `path`, an object `bounds` and a string `value`; one already carrying `value_chars` (a
/// slice) is left as it is.
pub(crate) fn bound_values(reply: &mut Value) {
    match reply {
        Value::Array(items) => items.iter_mut().for_each(bound_values),
        Value::Object(fields) => {
            if is_node(fields) && !fields.contains_key("value_chars") {
                cut(fields);
            }
            fields.values_mut().for_each(bound_values);
        }
        _ => {}
    }
}

fn is_node(fields: &serde_json::Map<String, Value>) -> bool {
    fields.get("path").is_some_and(Value::is_array)
        && fields.get("bounds").is_some_and(Value::is_object)
        && fields.get("value").is_some_and(Value::is_string)
}

fn cut(fields: &mut serde_json::Map<String, Value>) {
    let Some(Value::String(text)) = fields.get("value") else {
        return;
    };
    let total = text.chars().count();
    if total <= VALUE_CAP_CHARS {
        return;
    }
    let head: String = text.chars().take(VALUE_KEEP_CHARS).collect();
    let tail: String = text.chars().skip(total - VALUE_KEEP_CHARS).collect();
    fields.insert("value".into(), Value::String(format!("{head}…{tail}")));
    fields.insert("value_chars".into(), total.into());
    fields.insert("value_cut".into(), true.into());
}

/// Replaces the value of each shallowest node in `nodes` with the slice of `length` chars at
/// `offset`, clamped to the value. The shallowest nodes are the `within` root, or every node
/// sharing its path when two elements show the same one.
pub(crate) fn slice_roots(nodes: &mut Value, offset: u64, length: u64) {
    let Value::Array(items) = nodes else {
        return;
    };
    let depth = |node: &Value| node["path"].as_array().map_or(usize::MAX, Vec::len);
    let Some(shallowest) = items.iter().map(depth).min() else {
        return;
    };
    for node in items.iter_mut().filter(|n| depth(n) == shallowest) {
        let Some(Value::String(text)) = node.get("value") else {
            continue;
        };
        let total = text.chars().count();
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let take = usize::try_from(length).unwrap_or(usize::MAX);
        let slice: String = text.chars().skip(start).take(take).collect();
        let whole = slice.len() == text.len();
        node["value"] = slice.into();
        node["value_chars"] = total.into();
        node["value_offset"] = offset.into();
        node["value_cut"] = (!whole).into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(path: Value, value: &str) -> Value {
        json!({"path": path, "value": value, "bounds": {"x": 0, "y": 0, "w": 1, "h": 1},
               "visible": true, "depth": 0})
    }

    /// Three million chars whose position is readable from the text: the digit is i % 10.
    fn big() -> String {
        (0..3_000_000)
            .map(|i| char::from(b'0' + (i % 10) as u8))
            .collect()
    }

    #[test]
    fn a_multi_megabyte_value_is_cut_to_head_and_tail_with_its_length() {
        let text = big();
        let mut reply = json!({"clicked": node(json!(["big"]), &text), "settled": true});
        bound_values(&mut reply);
        let cut = &reply["clicked"];
        let expected = format!(
            "{}…{}",
            &text[..VALUE_KEEP_CHARS],
            &text[text.len() - VALUE_KEEP_CHARS..]
        );
        assert_eq!(cut["value"], json!(expected));
        assert_eq!(cut["value_chars"], json!(3_000_000));
        assert_eq!(cut["value_cut"], json!(true));
        assert_eq!(reply["settled"], json!(true));
    }

    #[test]
    fn values_up_to_the_cap_are_byte_identical() {
        for len in [0, 5, VALUE_CAP_CHARS] {
            let text = "é".repeat(len);
            let mut reply = json!({"nodes": [node(json!(["a"]), &text)]});
            let before = serde_json::to_string(&reply).unwrap();
            bound_values(&mut reply);
            assert_eq!(serde_json::to_string(&reply).unwrap(), before, "len {len}");
        }
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let text = "é".repeat(VALUE_CAP_CHARS + 1);
        let mut reply = json!({"nodes": [node(json!(["a"]), &text)]});
        bound_values(&mut reply);
        let node = &reply["nodes"][0];
        assert_eq!(node["value_chars"], json!(VALUE_CAP_CHARS + 1));
        let kept = node["value"].as_str().unwrap().chars().count();
        assert_eq!(kept, 2 * VALUE_KEEP_CHARS + 1);
    }

    #[test]
    fn nested_nodes_in_errors_are_cut_and_other_strings_are_not() {
        let text = big();
        let mut reply = json!({"code": "Ambiguous", "message": text.clone(),
                               "matches": [node(json!(["a"]), &text)]});
        bound_values(&mut reply);
        assert_eq!(reply["matches"][0]["value_cut"], json!(true));
        assert_eq!(reply["message"].as_str().unwrap().len(), text.len());
    }

    #[test]
    fn a_slice_of_a_multi_megabyte_value_is_exact() {
        let text = big();
        let mut reply = json!({"nodes": [node(json!(["big"]), &text),
                                         node(json!(["big", "child"]), &text)]});
        slice_roots(&mut reply["nodes"], 1_000_003, 12);
        bound_values(&mut reply);
        let root = &reply["nodes"][0];
        assert_eq!(root["value"], json!("345678901234"));
        assert_eq!(root["value_chars"], json!(3_000_000));
        assert_eq!(root["value_offset"], json!(1_000_003));
        assert_eq!(root["value_cut"], json!(true));
        assert_eq!(
            reply["nodes"][1]["value_cut"],
            json!(true),
            "the child is capped"
        );
        assert!(reply["nodes"][1].get("value_offset").is_none());
    }

    #[test]
    fn a_slice_is_clamped_to_the_value_and_whole_slices_are_not_cut() {
        let mut reply = json!([node(json!(["a"]), "héllo")]);
        slice_roots(&mut reply, 3, 100);
        assert_eq!(reply[0]["value"], json!("lo"));
        assert_eq!(reply[0]["value_cut"], json!(true));
        let mut past = json!([node(json!(["a"]), "héllo")]);
        slice_roots(&mut past, 9, 2);
        assert_eq!(past[0]["value"], json!(""));
        let mut whole = json!([node(json!(["a"]), "héllo")]);
        slice_roots(&mut whole, 0, 5);
        assert_eq!(whole[0]["value"], json!("héllo"));
        assert_eq!(whole[0]["value_chars"], json!(5));
        assert_eq!(whole[0]["value_cut"], json!(false));
    }

    #[test]
    fn a_slice_of_a_root_without_a_value_leaves_it_as_it_is() {
        let mut reply = json!([{"path": ["a"], "bounds": {}, "visible": true, "depth": 0}]);
        let before = reply.clone();
        slice_roots(&mut reply, 0, 5);
        assert_eq!(reply, before);
    }

    #[test]
    fn a_slice_reaches_every_root_on_a_path_tie() {
        let mut reply = json!([node(json!(["a"]), "abcdef"), node(json!(["a"]), "uvwxyz")]);
        slice_roots(&mut reply, 1, 2);
        assert_eq!(reply[0]["value"], json!("bc"));
        assert_eq!(reply[1]["value"], json!("vw"));
    }
}
