//! `PilotServer`: the seventeen MCP tools, each run against the one `Driver` on tokio's blocking pool.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use gpui_pilot_protocol::hover_wait_ms;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde_json::Value;

use crate::bound::{bound_values, slice_roots};
use crate::budget::shorten_tree;
use crate::driver::Driver;
use crate::error::ToolError;
use crate::params::{
    ClickArgs, ClipboardArgs, CloseArgs, CloseMode, DialogArgs, DragArgs, DropArgs, FramesArgs,
    HoverArgs, KeyArgs, LaunchArgs, PickFileArgs, ResizeArgs, ScreenshotArgs, ScrollArgs, TreeArgs,
    TypeArgs, ValueSlice, WaitArgs, structured,
};
use crate::redact::{scrub_tool_error, scrub_value};
use crate::summary::summary;

/// What a successful tool produced: its structured content and, for a screenshot, the PNG.
struct Done {
    value: Value,
    png: Option<Vec<u8>>,
    /// `tree`'s `value_slice`, applied to the scrubbed value so a slice never shows part of a
    /// secret.
    slice: Option<ValueSlice>,
    /// `tree`'s `max_chars`: shorten the reply only when the caller asked.
    max_chars: Option<usize>,
}

impl Done {
    fn value(value: Value) -> Done {
        Done {
            value,
            png: None,
            slice: None,
            max_chars: None,
        }
    }
}

/// Serves one client; every tool call takes the driver lock, so calls run one at a time.
#[derive(Clone)]
pub(crate) struct PilotServer {
    driver: Arc<Mutex<Driver>>,
    /// The driver's root, kept here so `pick_file` can reject bad arguments without the lock.
    root: Arc<PathBuf>,
    #[expect(dead_code, reason = "read through the macro-generated router")]
    tool_router: ToolRouter<Self>,
}

impl PilotServer {
    pub fn new(driver: Driver) -> PilotServer {
        PilotServer {
            root: Arc::new(driver.root().to_path_buf()),
            driver: Arc::new(Mutex::new(driver)),
            tool_router: Self::tool_router(),
        }
    }

    /// Runs `work` under the driver lock on the blocking pool, so no lock is held across an
    /// `.await`, and shapes its outcome as a success with a summary line or an `isError` result.
    async fn run(
        &self,
        tool: &'static str,
        work: impl FnOnce(&mut Driver) -> Result<Done, ToolError> + Send + 'static,
    ) -> Result<CallToolResult, ErrorData> {
        let driver = Arc::clone(&self.driver);
        let outcome = tokio::task::spawn_blocking(move || {
            // A panic in an earlier call leaves the driver usable; its state is still consistent.
            let mut guard = driver.lock().unwrap_or_else(PoisonError::into_inner);
            let outcome = work(&mut guard);
            (outcome, guard.secrets().to_vec())
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("{tool} failed: {e}"), None))?;
        let (outcome, secrets) = outcome;
        Ok(match outcome {
            Ok(done) => success_result(tool, done, &secrets),
            Err(error) => error_result(error, &secrets),
        })
    }

    /// Forwards a mapped request and reports its reply, waiting up to `budget_ms` plus margin.
    async fn forward(
        &self,
        tool: &'static str,
        request: gpui_pilot_protocol::Request,
        budget_ms: u64,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(tool, move |driver| {
            let reply = driver.call(request, Duration::from_millis(budget_ms))?;
            Ok(Done::value(structured(&reply)))
        })
        .await
    }
}

/// A success with its summary line. Scrubbing sees the full values, and both come before the
/// summary, so it quotes neither a redacted text nor an uncut value.
fn success_result(tool: &str, done: Done, secrets: &[String]) -> CallToolResult {
    let Done {
        mut value,
        png,
        slice,
        max_chars,
    } = done;
    scrub_value(&mut value, secrets);
    if let Some(slice) = slice {
        slice_roots(&mut value["nodes"], slice.offset, slice.length);
    }
    bound_values(&mut value);
    if let (true, Some(max_chars)) = (tool == "tree", max_chars) {
        shorten_tree(&mut value, max_chars);
    }
    let mut content = vec![ContentBlock::text(summary(tool, &value))];
    if let Some(png) = png {
        content.push(ContentBlock::image(STANDARD.encode(png), "image/png"));
    }
    let mut result = CallToolResult::structured(value);
    result.content = content;
    result
}

/// An `isError` result with remembered texts scrubbed and node values capped.
fn error_result(error: ToolError, secrets: &[String]) -> CallToolResult {
    let mut result = scrub_tool_error(error, secrets).into_result();
    if let Some(value) = &mut result.structured_content {
        bound_values(value);
    }
    result
}

/// A request's own budget in ms: its target timeout plus its settle, saturating so a huge
/// caller value cannot overflow.
fn budget(timeout_ms: u64, settle_ms: u64) -> u64 {
    timeout_ms.saturating_add(settle_ms)
}

/// A built `wait` request's budget: its own timeout, which for an elapsed wait is `ms`.
fn wait_budget(request: &gpui_pilot_protocol::Request) -> u64 {
    match request {
        gpui_pilot_protocol::Request::Wait { timeout_ms, .. } => *timeout_ms,
        _ => 0,
    }
}

#[tool_router]
impl PilotServer {
    #[tool(
        description = "Build and start the configured app, replacing a running one on request. The result's reused is true when the launch took a kept data dir (keep_data), so the app keeps its saved state. refresh_hz is the rate the display really runs at, in Hz (session: KWin's libxcvt-derived rate, e.g. 143.832 for 144)."
    )]
    async fn launch(
        &self,
        Parameters(args): Parameters<LaunchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run("launch", move |driver| driver.launch(args).map(Done::value))
            .await
    }

    #[tool(
        description = "Capture the window, or one element of it, as a PNG.",
        annotations(read_only_hint = true)
    )]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run("screenshot", move |driver| {
            let (value, png) = driver.screenshot(args)?;
            Ok(Done {
                value,
                png: Some(png),
                max_chars: None,
                slice: None,
            })
        })
        .await
    }

    #[tool(
        description = "List the app's elements with their paths, roles, names and bounds. A value over 2000 chars, here and in every other result, is cut to its first and last 400 with `value_chars` (its length) and `value_cut: true`; `value_slice: {offset, length}` with `within` returns that many chars of the `within` element's value from `offset` instead, adding `value_offset`. `max_chars` (off by default): when the reply is longer, runs of 4 or more similar siblings leave `nodes` and are summarised in a top-level `collapsed` array (count, first, last, parent), with a `shortened` note naming the `within` to use; `nodes` entries always keep the node shape.",
        annotations(read_only_hint = true)
    )]
    async fn tree(
        &self,
        Parameters(args): Parameters<TreeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let budget = Duration::from_millis(budget(args.timeout_ms, args.settle_ms));
        let slice = match args.slice() {
            Ok(slice) => slice,
            Err(error) => return Ok(error.into_result()),
        };
        let max_chars = args.max_chars;
        let request = args.request();
        self.run("tree", move |driver| {
            let mut done = Done::value(structured(&driver.call(request, budget)?));
            done.slice = slice;
            done.max_chars = max_chars;
            Ok(done)
        })
        .await
    }

    #[tool(
        description = "Click an element or a point, optionally holding modifiers (`[\"ctrl\"]` or `[\"shift\"]` for multi-select; needs a host built from kit-0.7.1-p12 or later, an older host is refused with Unsupported), then wait for the app to settle."
    )]
    async fn click(
        &self,
        Parameters(args): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let budget = budget(args.timeout_ms, args.settle_ms);
        self.forward("click", args.request(), budget).await
    }

    #[tool(
        name = "type",
        description = "Type text, optionally clicking an element first to focus it. Text typed into a masked or password input (headless, with a host built after kit-0.7.1-p12) or sent with `redact: true` is reported only as `<redacted N chars>` under `redacted`, and `masked` says the host saw a masked input."
    )]
    async fn type_text(
        &self,
        Parameters(args): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run("type", move |driver| {
            driver.type_text(args).map(Done::value)
        })
        .await
    }

    #[tool(
        description = "Press a keystroke such as `enter` or `ctrl-s`. A key of only modifiers (`shift`, `ctrl-shift`) is a modifier change, as on a desktop, so hover and a showing tooltip stay; any other key enters keyboard mode, which hides GPUI hover styles and tooltips until the pointer next moves, as in a real window."
    )]
    async fn key(
        &self,
        Parameters(args): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let budget = args.settle_ms;
        self.forward("key", args.request(), budget).await
    }

    #[tool(
        description = "Scroll over an element or a point by logical pixels (`dx`, `dy`) or by mouse wheel notches (`notches` down, `notches_x` right, `notch_ms` apart, default 16), one wheel event per notch as a real mouse wheel sends, optionally holding modifiers (`[\"ctrl\"]` for zoom). While `frames` records, use `settle_ms: 0` and a point target, since settle and id targets perturb timing."
    )]
    async fn scroll(
        &self,
        Parameters(args): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // The notches go out one by one before the settle wait begins.
        let budget = budget(args.timeout_ms, args.settle_ms).saturating_add(args.notch_time_ms());
        match args.request() {
            Ok(request) => self.forward("scroll", request, budget).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Move the pointer over an element or a point, then wait for the app to settle."
    )]
    async fn hover(
        &self,
        Parameters(args): Parameters<HoverArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // The backend waits out the tooltip delay before settling; 60 Hz is its longest wait.
        let budget = budget(args.timeout_ms, args.settle_ms)
            .saturating_add(hover_wait_ms(args.settle_ms, 60_000));
        self.forward("hover", args.request(), budget).await
    }

    #[tool(
        description = "Press on one element or point, move to another in steps, and release there, with the left button unless `button` is `right` or `middle` (to pan, say; needs a host built from kit-0.7.1-p15 or later), optionally holding modifiers (`[\"shift\"]`) from before the press until after the release (needs kit-0.7.1-p12 or later). An older host is refused with Unsupported."
    )]
    async fn drag(
        &self,
        Parameters(args): Parameters<DragArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Each step is one 16 ms frame, on top of the usual target and settle waits.
        let budget = budget(args.timeout_ms, args.settle_ms)
            .saturating_add(u64::from(args.steps).saturating_mul(16));
        match args.request() {
            Ok(request) => self.forward("drag", request, budget).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Drag files or folders from outside the window over an element or point and drop them there, as a file manager would: GPUI's external drop (entered, moved, dropped), seen by the app's `on_drop::<ExternalPaths>`. `paths` are absolute and need not exist. `cancel: true` leaves the window instead of dropping. Headless only (session mode answers Unsupported); needs a host built from kit-0.7.1-p17 or later, an older host is refused with Unsupported."
    )]
    async fn drop(
        &self,
        Parameters(args): Parameters<DropArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Two 16 ms frames pass between the drag's three events.
        let budget = budget(args.timeout_ms, args.settle_ms).saturating_add(32);
        match args.request() {
            Ok(request) => self.forward("drop", request, budget).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Resize the window's content to `width` by `height` logical pixels (each 64 to 8192), as a user dragging its edge would, without moving the pointer: hover and a showing tooltip are re-decided at the same point. Replies with the size the window really has, which can differ from the request when the app's minimum size forbids it. Headless refuses a side past 8192 device pixels at the window's scale. Session mode leaves maximised and fullscreen states, keeps the window's top-left and refuses a size larger than the output. Needs a host built from kit-0.7.1-p18 or later; an older host is refused with Unsupported."
    )]
    async fn resize(
        &self,
        Parameters(args): Parameters<ResizeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Session mode runs KWin scripts of up to 2 s each: a geometry read, the assignment, and
        // polls for the new frame within 2 s, the last of which may start just before that ends.
        let budget = args.settle_ms.saturating_add(8000);
        match args.request() {
            Ok(request) => self.forward("resize", request, budget).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Record the app's frames between `action: start` and `action: stop`; stop reports frame intervals, frames over the display's budget and input latency of wheel notches. While recording, scroll with `settle_ms: 0` and a point target, since settle and id targets perturb timing. `idle_ms` (start only, default 100) marks longer intervals as idle gaps."
    )]
    async fn frames(
        &self,
        Parameters(args): Parameters<FramesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Both actions answer at once; the driver's reply margin covers them.
        match args.request() {
            Ok(request) => self.forward("frames", request, 0).await,
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(
        description = "Read the GPUI prompt the app is waiting on, or answer it with a button label."
    )]
    async fn dialog(
        &self,
        Parameters(args): Parameters<DialogArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let budget = budget(args.timeout_ms, args.settle_ms);
        self.forward("dialog", args.request(), budget).await
    }

    #[tool(
        description = "Wait for an element to appear or, with `gone`, to disappear; or wait `ms` milliseconds.",
        annotations(read_only_hint = true)
    )]
    async fn wait(
        &self,
        Parameters(args): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        match args.request() {
            Ok(request) => {
                let budget = wait_budget(&request);
                self.forward("wait", request, budget).await
            }
            Err(error) => Ok(error.into_result()),
        }
    }

    #[tool(description = "Read the app's clipboard text, or replace it.")]
    async fn clipboard(
        &self,
        Parameters(args): Parameters<ClipboardArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let budget = args.settle_ms;
        self.forward("clipboard", args.request(), budget).await
    }

    #[tool(
        description = "Answer the app's open or save prompt with paths, or cancel it. In session mode it waits up to `timeout_ms` for the dialog, may copy inputs into the launch directory, and reports in `paths` the paths the app was given."
    )]
    async fn pick_file(
        &self,
        Parameters(args): Parameters<PickFileArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        match args.request(&self.root) {
            Ok(request) => {
                self.run("pick_file", move |driver| {
                    driver.pick_file(request).map(Done::value)
                })
                .await
            }
            Err(text) => Ok(CallToolResult::error(vec![ContentBlock::text(text)])),
        }
    }

    #[tool(
        description = "Close the running app. mode \"kill\" (default) ends it and reports its exit code and log tail; mode \"window\" closes its window as the window manager would, so the app may refuse (closed: false, its prompt in tree) or close (closed: true, then as kill). With keep_data its data dir and HOME stay for a later launch with keep_data."
    )]
    async fn close(
        &self,
        Parameters(args): Parameters<CloseArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run("close", move |driver| {
            match args.mode {
                CloseMode::Kill => driver.close(args.keep_data),
                CloseMode::Window => driver.close_window(args),
            }
            .map(Done::value)
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for PilotServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::WaitArgs;

    fn big_node(value: &str) -> gpui_pilot_protocol::Node {
        let rect = gpui_pilot_protocol::Rect::new(0.0, 0.0, 1.0, 1.0);
        let path = vec![gpui_pilot_protocol::Segment::Str("big".into())];
        let mut node = gpui_pilot_protocol::Node::new(path, rect, true, 0);
        node.value = Some(value.to_owned());
        node
    }

    #[test]
    fn a_success_is_scrubbed_then_capped_before_its_summary() {
        let text = format!("hunter22{}", "x".repeat(3_000_000));
        let node = serde_json::to_value(big_node(&text)).unwrap();
        let done = Done::value(serde_json::json!({"clicked": node, "hit": node}));
        let result = success_result("click", done, &["hunter22".to_owned()]);
        let value = result.structured_content.unwrap();
        let shown = value["clicked"]["value"].as_str().unwrap();
        assert!(
            shown.starts_with("<redacted 8 chars>xx"),
            "{}",
            &shown[..40]
        );
        assert_eq!(value["clicked"]["value_cut"], serde_json::json!(true));
        assert_eq!(
            value["clicked"]["value_chars"],
            serde_json::json!(3_000_018)
        );
        assert_eq!(value["hit"], value["clicked"]);
    }

    #[test]
    fn a_slice_straddling_a_secret_shows_only_its_placeholder() {
        let node = serde_json::to_value(big_node("xxhunter22yy")).unwrap();
        let mut done = Done::value(serde_json::json!({"nodes": [node]}));
        done.slice = Some(ValueSlice {
            offset: 4,
            length: 6,
        });
        let result = success_result("tree", done, &["hunter22".to_owned()]);
        let value = result.structured_content.unwrap();
        let node = &value["nodes"][0];
        // The scrubbed value is `xx<redacted 8 chars>yy`, 22 chars.
        assert_eq!(node["value"], serde_json::json!("edacte"), "{node}");
        assert_eq!(node["value_chars"], serde_json::json!(22));
    }

    fn thousands_of_rows() -> Value {
        let rows: Vec<_> = (0..4000)
            .map(|i| {
                serde_json::json!({"path": ["w", format!("body-line-{i}")],
                    "id": format!("body-line-{i}"), "role": "ListItem",
                    "bounds": {"x": 0, "y": i, "w": 9, "h": 1}, "visible": true, "depth": 1})
            })
            .collect();
        serde_json::json!({"nodes": rows})
    }

    #[test]
    fn a_default_tree_of_thousands_of_rows_is_the_uncapped_reply() {
        let reply = thousands_of_rows();
        let result = success_result("tree", Done::value(reply.clone()), &[]);
        let value = result.structured_content.unwrap();
        assert_eq!(value, reply);
        assert!(value.get("shortened").is_none() && value.get("collapsed").is_none());
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("body-line-3999"), "every node is listed");
        assert!(!text.contains("shortened"));
    }

    #[test]
    fn a_tree_with_max_chars_collapses_runs_only_into_the_collapsed_array() {
        let mut done = Done::value(thousands_of_rows());
        done.max_chars = Some(30_000);
        let result = success_result("tree", done, &[]);
        let value = result.structured_content.unwrap();
        assert!(value.to_string().chars().count() < 30_000);
        assert_eq!(value["nodes"].as_array().unwrap().len(), 0);
        assert_eq!(value["collapsed"][0]["count"], serde_json::json!(4000));
        assert_eq!(value["shortened"]["max_chars"], serde_json::json!(30_000));
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.len() < 2000, "{text}");
        assert!(
            text.contains("body-line-3999") && text.contains("within"),
            "{text}"
        );
    }

    #[test]
    fn a_shortened_tree_text_is_short_even_without_runs() {
        let nodes: Vec<_> = (0..180)
            .map(|i| {
                let path: Vec<String> = (0..12)
                    .map(|d| format!("{}-{i}-{d}", "s".repeat(50)))
                    .collect();
                serde_json::json!({"path": path, "id": format!("n{i}"), "role": format!("R{i}"),
                    "bounds": {"x": 0, "y": i, "w": 9, "h": 1}, "visible": true, "depth": 12})
            })
            .collect();
        let mut done = Done::value(serde_json::json!({"nodes": nodes}));
        done.max_chars = Some(30_000);
        let result = success_result("tree", done, &[]);
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.len() < 6000, "{}", text.len());
        assert!(
            text.contains("within") && text.contains("more nodes"),
            "{text}"
        );
    }

    #[test]
    fn only_tree_replies_are_shortened() {
        let mut done = Done::value(thousands_of_rows());
        done.max_chars = Some(30_000);
        let value = success_result("hover", done, &[])
            .structured_content
            .unwrap();
        assert_eq!(value["nodes"].as_array().unwrap().len(), 4000);
    }

    #[test]
    fn an_error_carrying_a_big_node_is_capped() {
        let mut error = gpui_pilot_protocol::PilotError::new(
            gpui_pilot_protocol::ErrorCode::Ambiguous,
            "two elements match",
        );
        error.matches = vec![big_node(&"y".repeat(3_000_000))];
        let result = error_result(error.into(), &[]);
        let value = result.structured_content.unwrap();
        assert_eq!(value["matches"][0]["value_cut"], serde_json::json!(true));
        assert!(value["matches"][0]["value"].as_str().unwrap().len() < 1000);
    }

    #[test]
    fn a_huge_budget_saturates_instead_of_overflowing() {
        assert_eq!(budget(u64::MAX, 2000), u64::MAX);
        assert_eq!(budget(5000, 2000), 7000);
    }

    #[test]
    fn an_elapsed_wait_budgets_its_ms_not_the_default_timeout() {
        let wait = |value| serde_json::from_value::<WaitArgs>(value).unwrap();
        let elapsed = wait(serde_json::json!({"ms": 20_000})).request().unwrap();
        assert_eq!(wait_budget(&elapsed), 20_000);
        let target = serde_json::json!({"target": {"id": ["x"]}, "timeout_ms": 700});
        let visible = wait(target).request().unwrap();
        assert_eq!(wait_budget(&visible), 700);
    }
}
