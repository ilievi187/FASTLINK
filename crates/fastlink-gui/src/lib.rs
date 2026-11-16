//! FASTLINK Linux desktop shell (pre-alpha).
//!
//! Milestone scope: read the versioned `fastlink devices --json` contract and
//! present the local device honestly, with a session-only visual-intent marker
//! that has no operational effect, and honest empty states for peers, models
//! and chat.
//!
//! This crate contains no network code, model download, inference or GPU use.

pub mod cli;
pub mod inventory;
pub mod state;
pub mod ui;
pub mod view;

/// Window title and short product name.
pub const APP_NAME: &str = "FASTLINK";
