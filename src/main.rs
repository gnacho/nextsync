//! NextSync - Nextcloud desktop client for GNOME, rewritten in Rust.
//!
//! Thin binary launcher; all logic lives in the `nextsync` library.

use nextsync::state::{AppState, StateController};

fn main() {
    let state = StateController::new(AppState::Unconfigured);
    println!(
        "NextSync (Rust) scaffold — state: {}",
        state.snapshot().state
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke_scaffold() {
        let state = nextsync::state::StateController::new(nextsync::state::AppState::Unconfigured);
        assert_eq!(
            state.snapshot().state,
            nextsync::state::AppState::Unconfigured
        );
    }
}
