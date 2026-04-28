//! `phpstan-lsp-bridge` — a small LSP server that wraps the PHPStan CLI and
//! publishes its findings as LSP diagnostics.
//!
//! This crate exposes its internals as a library so that the binary entry point
//! stays minimal and the units are individually testable.

pub mod config;
pub mod diagnostics;
pub mod error;
pub mod phpstan;
pub mod server;

pub use config::{BridgeConfig, DiagnosticTrigger};
pub use error::{BridgeError, BridgeResult};
