//! The private KDE session: the bwrap sandbox around it, and input, capture and the AT-SPI
//! tree inside it.

// Resolution and the agent return the wire's PilotError directly; it only travels on the cold failure path.
#![allow(clippy::result_large_err)]

pub mod agent;
mod clipboard;
pub mod cvt;
mod eis;
mod error;
pub mod isolation;
pub mod keymap;
pub mod portal;
mod refresh;
pub mod resolve;
pub mod sandbox;
pub mod session;
mod tree;

pub use error::SessionError;
pub use session::{Capture, PromptAnswer, Session, WindowGeometry};
