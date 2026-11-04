//! The `gpui-pilot` binary: an MCP server that drives a GPUI app's pilot host.

// ToolError carries a whole PilotError; it only travels on the cold failure path.
#![allow(clippy::result_large_err)]

mod backend;
mod bound;
mod budget;
mod config;
mod driver;
mod error;
mod launch;
mod params;
mod redact;
mod server;
mod stage;
mod summary;

use std::process::ExitCode;

use rmcp::ServiceExt;

use crate::driver::Driver;
use crate::launch::{Timeouts, temp_root};
use crate::server::PilotServer;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("mcp") | None => match serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("gpui-pilot: {error}");
                ExitCode::from(1)
            }
        },
        Some("session-agent") => gpui_pilot_session::agent::run(std::env::args_os().skip(2)),
        Some(other) => {
            eprintln!(
                "gpui-pilot: unknown command `{other}`; usage: gpui-pilot [mcp] | gpui-pilot session-agent [--env NAME=VALUE]... -- PROGRAM [ARG]..."
            );
            ExitCode::from(2)
        }
    }
}

/// Serves MCP over stdio until the client disconnects; dropping the server then drops the
/// driver, whose process group takes any running app with it.
fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::current_dir()?;
    let driver = Driver::new(
        root,
        temp_root(std::env::var_os("TMPDIR")),
        Timeouts::default(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        PilotServer::new(driver)
            .serve(rmcp::transport::stdio())
            .await?
            .waiting()
            .await?;
        Ok(())
    })
}
