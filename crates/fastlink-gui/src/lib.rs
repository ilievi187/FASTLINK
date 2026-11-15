//! FASTLINK Linux desktop shell (pre-alpha).
//!
//! Milestone scope: read the versioned `fastlink devices --json` contract and
//! present the local device honestly. An explicit **Discover devices** action
//! consumes the opt-in `fastlink.discover.v1` contract; discovery never starts
//! on launch or local refresh.
//!
//! This crate contains no network code, model download, inference or GPU use.

pub mod cli;
pub mod discovery;
pub mod inventory;
pub mod state;
pub mod ui;
pub mod view;

/// Window title and short product name.
pub const APP_NAME: &str = "FASTLINK";
