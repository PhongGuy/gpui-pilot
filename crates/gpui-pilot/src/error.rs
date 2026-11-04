//! The failed-tool-call shape: a protocol `PilotError` plus server-side detail fields.

use gpui_pilot_protocol::{ErrorCode, PilotError};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::summary::error_line;

/// A failed tool call: structured content is `{ code, message, ...details }`.
#[derive(Debug)]
pub(crate) struct ToolError {
    pub error: PilotError,
    pub details: Map<String, Value>,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        PilotError::new(code, message).into()
    }

    /// Adds one detail field next to `code` and `message`.
    pub fn with(mut self, key: &str, value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or(Value::Null);
        self.details.insert(key.to_owned(), value);
        self
    }

    #[cfg(test)]
    pub fn code(&self) -> ErrorCode {
        self.error.code
    }

    /// `isError: true`, the structured object, and one text block from `summary::error_line`.
    pub fn into_result(self) -> CallToolResult {
        let summary = error_line(&self.error);
        let mut value =
            serde_json::to_value(&self.error).unwrap_or_else(|_| Value::Object(Map::new()));
        if let Value::Object(fields) = &mut value {
            fields.extend(self.details);
        }
        // structured_error puts the whole JSON in the text block; keep one summary line instead.
        let mut result = CallToolResult::structured_error(value);
        result.content = vec![ContentBlock::text(summary)];
        result
    }
}

impl From<PilotError> for ToolError {
    fn from(error: PilotError) -> Self {
        Self {
            error,
            details: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_pilot_protocol::Segment;
    use serde_json::json;

    #[test]
    fn an_error_result_carries_code_message_and_details() {
        let result = ToolError::new(ErrorCode::BuildFailed, "build exited with status 3")
            .with("log_tail", "one\ntwo")
            .into_result();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content,
            Some(
                json!({"code": "BuildFailed", "message": "build exited with status 3", "log_tail": "one\ntwo"})
            )
        );
        let text: Vec<_> = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.clone())
            .collect();
        assert_eq!(text, ["BuildFailed: build exited with status 3"]);
    }

    #[test]
    fn a_host_error_keeps_its_nearest_ids() {
        let mut error = PilotError::new(ErrorCode::ElementNotFound, "no element `cont`");
        error.nearest = vec![vec![Segment::Str("count".into())]];
        let value = ToolError::from(error)
            .into_result()
            .structured_content
            .unwrap();
        assert_eq!(value["code"], json!("ElementNotFound"));
        assert_eq!(value["nearest"], json!([["count"]]));
    }
}
