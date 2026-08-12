//! Path resolution.
//!
//! Fase 1: resolve config and data directories (mirroring the Python
//! `util/paths.py`).

/// Placeholder for path helpers.
pub struct Paths;

impl Paths {
    /// Returns the config directory path.
    ///
    /// Placeholder: `~/.config/nextsync` until Fase 1 lands.
    pub fn config_dir() -> &'static str {
        "~/.config/nextsync"
    }
}
