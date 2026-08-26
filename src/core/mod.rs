//! Core non-UI services: scheduling, triggers, watchers, guards and
//! desktop integration. Fases 2, 3 and 5.

pub mod account_runtime;
pub mod autostart;
pub mod conflict_files;
pub mod debounce;
pub mod delete_guard;
pub mod desktop_integration;
pub mod etag_store;
pub mod exclusions;
pub mod files_journal;
pub mod log;
pub mod network;
pub mod notifications;
pub mod pending_changes;
pub mod power;
pub mod proc_scan;
pub mod scheduler;
pub mod server_notifications;
pub mod suspend;
pub mod sync_permit;
pub mod sync_safety;
pub mod triggers;
pub mod updates;
pub mod watcher;
