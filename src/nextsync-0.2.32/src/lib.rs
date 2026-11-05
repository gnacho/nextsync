//! NextSync - Nextcloud desktop client for GNOME, rewritten in Rust.
//!
//! Thin desktop layer over the `nextcloudcmd` sync engine: configuration,
//! credentials, scheduling, watchers, tray, windows, logs and conflict
//! resolution. See `plans/2026-08-13-rust-rewrite.md` for the roadmap.
//!
//! Exposed as a library so every placeholder module is part of the public
//! API (no dead-code warnings) and the `nextsync` binary stays a thin
//! launcher.

pub mod core;
pub mod nextcloud;
pub mod state;
pub mod storage;
pub mod ui;
pub mod util;
