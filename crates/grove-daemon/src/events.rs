//! The event bus — one in-process fan-out carrying [`grove_api::Event`].
//!
//! This is what v1's `Phoenix.PubSub` `"roots"` topic was: engines and the watcher
//! publish, and everything that wants to know subscribes. Two consumers exist
//! today — the engine set (which diffs `roots_changed` against its running engines)
//! and the `GET /api/events` SSE route — and both want *every*
//! subscriber to see every event, which is `broadcast`, not `mpsc`.
//!
//! **Bounded, and it drops rather than blocks.** Every event is level-triggered —
//! "something changed, re-read it" — so a subscriber that falls behind loses
//! freshness, never correctness: tokio hands it a `Lagged(n)` and the fix is to
//! re-read the world, which is exactly what a slow consumer had to do anyway. The
//! opposite choice (an unbounded queue, or publishers blocking on the slowest
//! reader) would let a stalled SSE client apply backpressure all the way into the
//! engine that produced the event.

use grove_api::Event;
use tokio::sync::broadcast;

/// How many events a lagging subscriber may fall behind before it is told to
/// re-read instead. Sized for a burst: one boot of a large home publishes a
/// `roots_changed` plus a started/finished pair per root.
pub const CAPACITY: usize = 256;

/// A handle on the bus. Cheap to clone; publishing from a clone reaches every
/// subscriber of the original.
#[derive(Clone, Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// A bus with [`CAPACITY`] slots.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(CAPACITY).0,
        }
    }

    /// Publish to every current subscriber.
    ///
    /// Never fails and never blocks: `broadcast::Sender::send` errors only when
    /// nobody is listening, which is the normal state of a daemon with no UI
    /// attached — an event nobody wanted is not a fault.
    pub fn publish(&self, event: Event) {
        // The bus is typed on the whole frame vocabulary because everything it
        // carries goes out one wire; only the four state events belong *on* it. A
        // snapshot here would be one connection's answer fanned out to every other
        // connection, and a log line would evict the state events it shares this
        // buffer with — see `grove_api::Event::is_state`.
        debug_assert!(
            event.is_state(),
            "only state events belong on the bus: {}",
            event.name()
        );
        let _ = self.tx.send(event);
    }

    /// A receiver seeing every event published from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// How many subscribers are attached — the daemon's own introspection, and how
    /// a test asserts it is actually listening before it triggers work.
    #[must_use]
    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, EventBus};
    use grove_api::events::{Event, TaskKind, TaskOutcome};
    use tokio::sync::broadcast::error::TryRecvError;

    fn roots(slug: &str) -> Event {
        Event::RootsChanged {
            roots: vec![slug.into()],
        }
    }

    #[tokio::test]
    async fn every_subscriber_sees_every_event() {
        let bus = EventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        assert_eq!(bus.subscribers(), 2);

        bus.publish(roots("o/r"));
        bus.publish(Event::TaskStarted {
            slug: "o/r".into(),
            kind: TaskKind::Reconcile,
        });

        for rx in [&mut a, &mut b] {
            assert_eq!(rx.recv().await.unwrap(), roots("o/r"));
            assert!(matches!(
                rx.recv().await.unwrap(),
                Event::TaskStarted { .. }
            ));
        }
    }

    /// Publishing into the void is the daemon's ordinary state — no UI attached —
    /// and must not be an error the producer has to handle.
    #[tokio::test]
    async fn publishing_with_no_subscribers_is_not_a_failure() {
        let bus = EventBus::new();
        bus.publish(roots("o/r"));
        assert_eq!(bus.subscribers(), 0);
    }

    /// A subscriber that stops reading loses the overflow rather than stalling the
    /// producer: `Lagged` tells it to re-read, and the newest events are still there
    /// behind it.
    #[tokio::test]
    async fn a_lagging_subscriber_is_told_to_re_read_and_keeps_receiving() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        for n in 0..CAPACITY + 2 {
            bus.publish(Event::TaskFinished {
                slug: format!("o/{n}"),
                kind: TaskKind::Fill,
                outcome: TaskOutcome::Ok,
            });
        }

        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Lagged(n)) if n == 2),
            "the overflow is reported, not silently dropped"
        );
        assert!(
            rx.try_recv().is_ok(),
            "and the subscriber keeps receiving from the newest events"
        );
    }
}
