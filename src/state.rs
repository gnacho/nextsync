//! Application state and state machine.
//!
//! Fase 2: the full state machine lives here.

/// Placeholder for the state controller.
///
/// Fase 2 (Task 2.1): `enum AppState` (IDLE / SYNCING / PAUSED /
/// DELETE_REVIEW...), a `StateController` with `async_channel` subscriptions
/// and an `AggregateStateController` for multi-folder aggregation.
pub struct StateController;

impl StateController {
    /// Reports whether the state controller is ready.
    ///
    /// Placeholder: always `true` until Fase 2 lands.
    pub fn ready() -> bool {
        true
    }
}
