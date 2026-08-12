//! `nextcloudcmd` command wrapper.
//!
//! Fase 2 (Task 2.3): build `std::process::Command` args (`--trust`,
//! `--httpproxy`, `--max-sync-retries`, `--exclude`, `--path`).

/// Placeholder for the `nextcloudcmd` command builder.
pub struct Command;

impl Command {
    /// Returns the name of the wrapped binary.
    ///
    /// Placeholder: constant until Fase 2 lands.
    pub fn binary_name() -> &'static str {
        "nextcloudcmd"
    }
}
