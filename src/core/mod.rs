//! Core non-UI services: scheduling, triggers, watchers, guards and
//! desktop integration. Fases 2, 3 and 5.

pub mod account_runtime;
pub mod autostart;
pub mod debounce;
pub mod delete_guard;
pub mod desktop_integration;
pub mod exclusions;
pub mod network;
pub mod power;
pub mod scheduler;
pub mod suspend;
pub mod sync_permit;
pub mod triggers;
pub mod updates;
pub mod watcher;
