//! The headless pilot host: runs a GPUI app's root view on a pilot-owned platform and serves
//! the gpui-pilot JSON-lines protocol for it.

// Handlers return the wire's PilotError directly; it only travels on the cold failure path.
#![allow(clippy::result_large_err)]

#[cfg(not(debug_assertions))]
compile_error!("gpui-pilot-host is debug-only: it enables gpui-kit/test-support");

mod dispatch;
mod executor;
mod input;
mod layers;
mod platform;
mod resolve;
mod server;
mod settle;
mod window;

pub use executor::Pilot;
pub use input::{
    MAX_NOTCHES, NOTCH_MS_RANGE, NotchEvents, click_events, dispatch_events, dispatch_keystrokes,
    drag_move, drag_path, drag_press, drag_release, hover_events, notch_events, parse_keys,
    scroll_events, text_keystrokes,
};
pub use layers::{Layer, Layering, a11y_only, node_id};
pub use platform::{PathPrompt, PilotPlatform, PromptAnswered, Recorded};
pub use resolve::{
    Actionable, Observed, Resolved, actionable, focused_input_masked, observe, resolve,
    segment_matches, segment_of, subtree, to_nodes,
};
pub use server::{Launch, PilotApp, run};
pub use settle::{IDLE_MS, QUIET_LAYOUT_MS, SettleTracker, Settled, settle, ticks_for};
pub use window::{Dialog, Frame, PilotWindow};
