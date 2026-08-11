//! Task lifecycle event bus, port of `src/main/buddy/events.ts`.

use crate::buddy::types::TaskEventEnvelope;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Broadcast pub/sub for task events. The runner/store publish; the Tauri
/// setup forwards every envelope to the frontend as a `buddy:event` emit.
#[derive(Clone)]
pub struct BuddyEventBus {
    sender: Arc<broadcast::Sender<TaskEventEnvelope>>,
}

impl Default for BuddyEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl BuddyEventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(1024);
        BuddyEventBus {
            sender: Arc::new(sender),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskEventEnvelope> {
        self.sender.subscribe()
    }

    /// Fire-and-forget; mirrors the synchronous publish of the Electron bus.
    pub fn publish(&self, event: TaskEventEnvelope) {
        // Ignore send errors: they only mean there are no subscribers.
        let _ = self.sender.send(event);
    }
}
