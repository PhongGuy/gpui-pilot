//! Keeps text typed by `type` out of everything the server sends back, so a client that logs
//! its calls and replies never writes a password: the text becomes `<redacted N chars>`.

use gpui_pilot_protocol::{ErrorCode, Reply, redacted};
use serde_json::{Value, json};

use crate::error::ToolError;
use crate::params::structured;

/// The structured result of a `type` reply; it names the text only as a placeholder, under
/// `redacted`, when the caller asked for it or the host reports the focused input masked.
pub(crate) fn typed_result(reply: &Reply, text: &str, redact: bool) -> Value {
    let mut value = structured(reply);
    if redact || matches!(reply, Reply::Type { masked: true, .. }) {
        value["redacted"] = json!(redacted(text));
    }
    value
}

/// Remembered texts shorter than this are not scrubbed from later replies: a one-letter
/// secret would mangle every name. The `type` call's own reply is redacted at any length.
pub(crate) const MIN_SCRUB_CHARS: usize = 4;

/// Fields that name elements rather than show their text; scrubbing them would break targets.
const IDENTIFIER_KEYS: [&str; 5] = ["id", "path", "role", "nearest", "code"];

/// The remembered texts worth scrubbing, longest first so a text containing another wins.
fn scrubbable(secrets: &[String]) -> Vec<&str> {
    let mut kept: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|s| s.chars().count() >= MIN_SCRUB_CHARS)
        .collect();
    kept.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    kept
}

/// `text` with each secret replaced by its placeholder in one left-to-right pass, copying any
/// placeholder already present untouched so placeholders never nest.
fn scrub_text(text: &str, secrets: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(len) = placeholder_len(rest) {
            out.push_str(&rest[..len]);
            rest = &rest[len..];
        } else if let Some(secret) = secrets.iter().find(|s| rest.starts_with(**s)) {
            out.push_str(&redacted(secret));
            rest = &rest[secret.len()..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// The byte length of an exact `<redacted N char>` or `<redacted N chars>` at the start of
/// `text`, so only a real placeholder is copied untouched.
fn placeholder_len(text: &str) -> Option<usize> {
    let after = text.strip_prefix("<redacted ")?;
    let digits = after.bytes().take_while(u8::is_ascii_digit).count();
    let tail = &after[digits..];
    let suffix = [" chars>", " char>"]
        .into_iter()
        .find(|s| tail.starts_with(s))?;
    (digits > 0).then(|| text.len() - after.len() + digits + suffix.len())
}

/// `value` with every remembered text in its free-text strings replaced by its placeholder.
pub(crate) fn scrub_value(value: &mut Value, secrets: &[String]) {
    let secrets = scrubbable(secrets);
    if !secrets.is_empty() {
        scrub_with(value, &secrets);
    }
}

fn scrub_with(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::String(text) => *text = scrub_text(text, secrets),
        Value::Array(items) => items.iter_mut().for_each(|v| scrub_with(v, secrets)),
        Value::Object(fields) => fields
            .iter_mut()
            .filter(|(key, _)| !IDENTIFIER_KEYS.contains(&key.as_str()))
            .for_each(|(_, v)| scrub_with(v, secrets)),
        _ => {}
    }
}

/// `error` with every remembered text in its message, node names and values, and details
/// replaced; codes, paths and nearest ids are left as they are.
pub(crate) fn scrub_tool_error(mut error: ToolError, secrets: &[String]) -> ToolError {
    let kept = scrubbable(secrets);
    if kept.is_empty() {
        return error;
    }
    let inner = &mut error.error;
    inner.message = scrub_text(&inner.message, &kept);
    for node in inner.matches.iter_mut().chain(inner.hit.as_mut()) {
        for text in [&mut node.name, &mut node.value].into_iter().flatten() {
            *text = scrub_text(text, &kept);
        }
    }
    error
        .details
        .values_mut()
        .for_each(|v| scrub_with(v, &kept));
    error
}

/// A failed redacted `type` with the text, or a character of it, taken out of its message.
pub(crate) fn scrub_error(mut error: ToolError, text: &str) -> ToolError {
    // The session keymap names the one character it cannot type, so that message is replaced
    // whole; any other message gets the placeholder wherever it quotes the text.
    error.error.message = if error.error.code == ErrorCode::UnsupportedCharacter {
        format!(
            "the session keymap cannot type a character of the {}",
            redacted(text)
        )
    } else if text.is_empty() {
        error.error.message
    } else {
        scrub_text(&error.error.message, &[text])
    };
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::{PilotError, Segment};

    const SECRET: &str = "hünter2";

    fn reply(masked: bool) -> Reply {
        Reply::Type {
            settled: true,
            masked,
        }
    }

    #[test]
    fn the_placeholder_counts_characters_not_bytes() {
        assert_eq!(redacted(SECRET), "<redacted 7 chars>");
        assert_eq!(redacted("x"), "<redacted 1 char>");
    }

    #[test]
    fn a_masked_input_or_an_explicit_request_redacts_and_nothing_else_does() {
        let masked = json!({"settled": true, "masked": true, "redacted": "<redacted 7 chars>"});
        assert_eq!(typed_result(&reply(true), SECRET, false), masked);
        assert_eq!(
            typed_result(&reply(false), SECRET, true),
            json!({"settled": true, "redacted": "<redacted 7 chars>"})
        );
        assert_eq!(
            typed_result(&reply(false), SECRET, false),
            json!({"settled": true})
        );
    }

    #[test]
    fn later_replies_quote_redacted_texts_only_as_placeholders() {
        let secrets = vec!["abcd".to_owned(), "abcdefgh".to_owned()];
        let mut value = json!({
            "nodes": [{"value": "xabcdefghy", "name": "abcd abcd"}],
            "count": 3,
        });
        scrub_value(&mut value, &secrets);
        assert_eq!(
            value,
            json!({
                "nodes": [{
                    "value": "x<redacted 8 chars>y",
                    "name": "<redacted 4 chars> <redacted 4 chars>",
                }],
                "count": 3,
            })
        );
    }

    #[test]
    fn short_remembered_texts_are_not_scrubbed_from_later_replies() {
        let secrets = vec!["e".to_owned(), "abc".to_owned()];
        let mut value = json!({"nodes": [{"path": ["name"], "name": "Save abc", "value": "e"}]});
        let before = value.clone();
        scrub_value(&mut value, &secrets);
        assert_eq!(value, before);
    }

    #[test]
    fn identifier_fields_are_never_scrubbed() {
        let secrets = vec!["count".to_owned()];
        let mut value = json!({
            "nodes": [{"id": "count", "path": ["root", "count"], "role": "count",
                       "name": "count me"}],
            "nearest": [["count"]],
        });
        scrub_value(&mut value, &secrets);
        assert_eq!(
            value,
            json!({
                "nodes": [{"id": "count", "path": ["root", "count"], "role": "count",
                           "name": "<redacted 5 chars> me"}],
                "nearest": [["count"]],
            })
        );
    }

    #[test]
    fn only_the_exact_placeholder_form_is_copied_untouched() {
        let secrets = vec!["hunter2".to_owned()];
        let mut value =
            json!("<redacted hunter2> <redacted 7 chars> <redacted 1 char> <redacted x>");
        scrub_value(&mut value, &secrets);
        assert_eq!(
            value,
            json!(
                "<redacted <redacted 7 chars>> <redacted 7 chars> <redacted 1 char> <redacted x>"
            )
        );
    }

    #[test]
    fn a_secret_inside_a_placeholder_is_left_alone() {
        let secrets = vec!["abcd".to_owned(), "redacted".to_owned(), "chars".to_owned()];
        let mut value = json!("abcd then <redacted 9 chars> then redacted");
        scrub_value(&mut value, &secrets);
        assert_eq!(
            value,
            json!("<redacted 4 chars> then <redacted 9 chars> then <redacted 8 chars>")
        );
    }

    #[test]
    fn an_error_is_scrubbed_even_when_a_secret_matches_its_code_or_ids() {
        let mut error = PilotError::new(
            ErrorCode::ElementNotFound,
            "no element path matches [\"Element\"] near Element",
        );
        error.nearest = vec![vec![Segment::Str("Element".into())]];
        let scrubbed = scrub_tool_error(error.into(), &["Element".to_owned()]);
        assert_eq!(scrubbed.error.code, ErrorCode::ElementNotFound);
        assert_eq!(
            scrubbed.error.message,
            "no element path matches [\"<redacted 7 chars>\"] near <redacted 7 chars>"
        );
        assert_eq!(
            scrubbed.error.nearest,
            vec![vec![Segment::Str("Element".into())]]
        );
    }

    #[test]
    fn an_error_is_not_mangled_by_a_short_secret() {
        let message = "no element path matches [\"nope\"]";
        let error = ToolError::new(ErrorCode::ElementNotFound, message);
        let scrubbed = scrub_tool_error(error, &["e".to_owned()]);
        assert_eq!(scrubbed.error.message, message);
    }

    #[test]
    fn an_unsupported_character_error_no_longer_names_the_character() {
        let error = ToolError::new(
            ErrorCode::UnsupportedCharacter,
            "the session keymap cannot type 'ü'",
        );
        let scrubbed = scrub_error(error, SECRET);
        assert_eq!(scrubbed.error.code, ErrorCode::UnsupportedCharacter);
        assert!(
            !scrubbed.error.message.contains('ü'),
            "{}",
            scrubbed.error.message
        );
    }

    #[test]
    fn an_error_quoting_the_text_quotes_the_placeholder_instead() {
        let error = ToolError::new(ErrorCode::Internal, format!("failed typing {SECRET:?}"));
        let scrubbed = scrub_error(error, SECRET);
        assert_eq!(
            scrubbed.error.message,
            "failed typing \"<redacted 7 chars>\""
        );
    }
}
