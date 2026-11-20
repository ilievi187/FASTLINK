//! FASTLINK Linux desktop shell (pre-alpha).
//!
//! Milestone scope: read the versioned `fastlink devices --json` contract and
//! present the local device honestly, with a session-only visual-intent marker
//! that has no operational effect. An explicit **Discover devices** action
//! consumes the opt-in `fastlink.discover.v1` contract; discovery never starts
//! on launch or local refresh. A session catalog can hold several explicit
//! `.gguf` paths. Each registration checks one path on a background worker and
//! stores metadata only. A symlink keeps the canonical name. The draft is
//! cleared after success, and painted names drop control and bidi characters.
//!
//! This crate contains no network code, model download, inference or GPU use.

pub mod cli;
pub mod discovery;
pub mod inventory;
pub mod models;
pub mod state;
pub mod ui;
pub mod view;

/// Window title and short product name.
pub const APP_NAME: &str = "FASTLINK";
