//! Accordo's configuration, process supervision, and terminal presentations.

#[cfg(not(unix))]
compile_error!("Accordo currently supports macOS and Linux (Unix process groups are required).");

pub mod app;
pub mod config;
pub mod logs;
pub mod pty;
pub mod supervisor;
pub mod terminal;
pub mod view;
