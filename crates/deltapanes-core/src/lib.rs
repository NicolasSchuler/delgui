//! Frontend-agnostic core: invoke `delta`, parse what it prints.
//!
//! Everything here is deliberately free of any GUI dependency so the egui
//! frontend and the ratatui fallback can share it.

pub mod ansi;
pub mod config;
pub mod delta;
pub mod language;
pub mod watch;

pub use ansi::{Color, Line, Span, Style};
pub use delta::{Delta, DeltaError, Input, Options};
