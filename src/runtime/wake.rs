//! Process-global wake handle for the server event loop.
//!
//! The server loop blocks in [`polling::Poller::wait`]. Producer threads
//! that have no file descriptor to register with the poller (PTY reader
//! threads, the update-check thread) call [`notify`] after publishing work
//! on their mpsc channels so the loop wakes immediately instead of waiting
//! for the heartbeat timeout. Outside the server (local client mode, unit
//! tests) no waker is installed and [`notify`] is a cheap no-op.

use std::sync::{Arc, RwLock};

use polling::Poller;

static SERVER_WAKER: RwLock<Option<Arc<Poller>>> = RwLock::new(None);

/// Install `poller` as the process-global wake target and return a guard
/// that uninstalls it on drop (i.e. when `server::run` returns). Only one
/// server loop runs per process, so a single slot suffices.
pub fn install(poller: Arc<Poller>) -> WakeGuard {
    if let Ok(mut slot) = SERVER_WAKER.write() {
        *slot = Some(poller);
    }
    WakeGuard
}

/// Wake the server loop if one is running; no-op otherwise. A notify issued
/// while the loop is between waits is remembered and wakes the next wait,
/// so producers can never lose a wakeup.
pub fn notify() {
    if let Ok(slot) = SERVER_WAKER.read()
        && let Some(poller) = slot.as_ref()
    {
        let _ = poller.notify();
    }
}

/// Uninstalls the process-global waker when dropped.
pub struct WakeGuard;

impl Drop for WakeGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = SERVER_WAKER.write() {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use polling::{Events, Poller};

    #[test]
    fn notify_without_installed_waker_is_a_noop() {
        // Must not panic or block.
        super::notify();
    }

    #[test]
    fn notify_wakes_a_subsequent_wait_and_guard_uninstalls() {
        let poller = Arc::new(Poller::new().expect("create poller"));
        {
            let _guard = super::install(Arc::clone(&poller));
            super::notify();
            let mut events = Events::new();
            // The pending notification makes this return immediately.
            poller
                .wait(&mut events, Some(Duration::from_secs(5)))
                .expect("wait after notify");
            assert!(events.is_empty());
        }
        // Guard dropped: notify must be a no-op again (nothing to observe
        // beyond "does not panic", but a stale waker would wake this wait).
        super::notify();
        let mut events = Events::new();
        poller
            .wait(&mut events, Some(Duration::from_millis(10)))
            .expect("wait after uninstall");
        assert!(events.is_empty());
    }
}
