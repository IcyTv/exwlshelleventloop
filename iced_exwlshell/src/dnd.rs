//! Copy-only drag-and-drop events and source data.
//!
//! Source identifiers are supplied by the application in [`SourceData`]. Offer
//! identifiers come from incoming [`Event`] values. These are separate identity
//! spaces: use source identifiers for cancellation and offer identifiers for
//! acceptance, receipt, and completion. Successful drops never request a move.

use std::sync::Arc;

pub use exwlshellev::dnd::{Event, SourceData};

/// A shared callback for drag-and-drop events.
///
/// The callback runs on the compositor owner thread and must not block it.
/// Forward events to application work queues; read payloads and perform file
/// operations outside the callback.
/// Return false when a critical event cannot be queued: the backend rejects the
/// affected offer/source instead of leaving an unowned transfer alive. Motion
/// events may be coalesced by returning true without queueing every position.
#[derive(Clone)]
pub struct Listener(Arc<dyn Fn(Event) -> bool + Send + Sync>);

impl Listener {
    /// Create a listener whose callback must not block the compositor owner.
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(Event) -> bool + Send + Sync + 'static,
    {
        Self(Arc::new(callback))
    }

    pub(crate) fn send(&self, event: Event) -> bool {
        (self.0)(event)
    }
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener").finish_non_exhaustive()
    }
}
