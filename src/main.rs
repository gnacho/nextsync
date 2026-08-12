//! NextSync - Nextcloud desktop client for GNOME, rewritten in Rust.
//!
//! Thin binary launcher; all logic lives in the `nextsync` library.

use nextsync::state::StateController;

fn main() {
    println!("NextSync (Rust) scaffold");
    println!("state ready: {}", StateController::ready());
}

#[cfg(test)]
mod tests {
    #[test]
    fn smoke_scaffold() {
        assert!(nextsync::state::StateController::ready());
    }
}
