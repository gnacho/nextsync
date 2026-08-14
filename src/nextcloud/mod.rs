//! Nextcloud integration: `nextcloudcmd` wrapper, credentials, sync engine
//! and push protocol. Fases 1, 2 and 4.

pub mod api;
pub mod command;
pub mod credentials;
pub mod driver;
pub mod login_flow;
pub mod nextcloudcmd_progress;
pub mod push;
pub mod push_protocol;
pub mod sync_engine;
