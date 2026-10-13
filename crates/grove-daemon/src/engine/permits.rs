//! The clone bound: [`DEFAULT_CLONE_LIMIT`](super::DEFAULT_CLONE_LIMIT) permits shared
//! across every root, and the one registry of drivers waiting for one.
//!
//! A permit is only ever **tried** — at the head of a root's lane, where waiting would
//! hold the lane (see `Driver::dispatch`). A driver whose try failed
//! parks its mailbox here, holding nothing, and a freed permit **pokes** the driver at
//! the front of the queue, which re-dispatches into the same try. A poke is a wake-up,
//! never a claim: the reconcile it re-drives may still have to wait out its own lane,
//! and a reconcile waiting for a lane holds no permit.
//!
//! Why a registry rather than a `Semaphore` with off-lane waiters: a FIFO semaphore
//! whose waiters drop the permit as a wake-up (a wake-up must not keep it) passes that
//! permit waiter to waiter, and each one re-dispatches a try that loses — one release
//! wakes every parked driver instead of one.
//!
//! ## One poke per free permit
//!
//! `poked` counts pokes in flight, and every state change ends by poking parked drivers
//! until `poked == free` or none is left. Each poke ends in exactly one of two ways —
//! consumed by the re-dispatched try ([`ClonePermits::take`]) or declined by dropping
//! it (a driver that is gone, busy, or no longer needs a clone) — and both decrement
//! `poked` under the lock and rebalance. So a free permit with a parked driver always
//! has a poke in flight, and a release wakes exactly one driver, not every waiter.
//!
//! Not a fair queue: a *fresh* dispatch, one not poked, may take a free permit ahead of
//! the poked driver it was meant for. That driver's try then fails and it parks again
//! at the back — one re-park per steal, never a circuit.
//!
//! A poked reconcile queued behind foreground work on its own lane carries its poke
//! until admitted, so the next parked root waits out that op. The permit itself stays
//! free for any fresh try meanwhile; only the wake-up order is delayed.
//!
//! Pokes are sent after the lock is released, and nothing that can drop a [`Poke`], a
//! [`ClonePermit`] or a mailbox's last handle runs under it: the first two re-lock in
//! `Drop`, and the last handle — strong *or* weak, each holds the channel — drops any
//! message stranded in the mailbox, possibly a `Poke` or a `ClonePermit` riding a
//! finished op, so it re-locks too. The one handle touched under the lock is the
//! temporary sender [`park`](ClonePermits::park) upgrades to compare, never the last:
//! the weak entry it came from still holds the channel.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;

use super::Msg;

/// The shared clone bound and its waiter registry. One per daemon, in [`super::Deps`].
pub struct ClonePermits {
    state: Mutex<State>,
}

struct State {
    free: usize,
    /// Drivers whose try failed, oldest first, **at most one entry per driver**.
    ///
    /// tokio's receiver drains once on drop, so a send racing that drain strands its
    /// message in the channel until the channel itself goes — when its last handle,
    /// strong or weak, is dropped. A stranded `Poke` held there is never declined and
    /// `poked` sticks high. So a driver popped to carry a poke must leave nothing here
    /// that addresses its mailbox: [`park`](ClonePermits::park) dedupes, and once the
    /// one entry is popped, a stranded poke is dropped — declined — as soon as the
    /// transient strong holders go (the driver's own sender, `Engine` handles,
    /// in-flight dispatch tasks). Weak as well, so a dead driver's entry still waiting
    /// its turn does not pin its mailbox; one that no longer upgrades declines its poke.
    parked: VecDeque<mpsc::WeakUnboundedSender<Msg>>,
    /// Pokes sent and not yet consumed or declined. Never above `free` for long: a
    /// steal can leave it there until the robbed driver's try consumes its poke.
    poked: usize,
    /// Pokes ever sent, for the contention probe in `tests/engine.rs`.
    pokes: u64,
}

/// One held permit; dropping it frees the slot and pokes a parked driver.
pub struct ClonePermit(Arc<ClonePermits>);

/// A freed permit's wake-up, carried by [`Msg::PermitFreed`] into the re-dispatched
/// try. Dropped unconsumed, it declines, and the next parked driver is poked instead.
pub(super) struct Poke(Option<Arc<ClonePermits>>);

impl ClonePermits {
    /// A bound of `limit` concurrent clones.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State {
                free: limit,
                parked: VecDeque::new(),
                poked: 0,
                pokes: 0,
            }),
        }
    }

    /// Permits not held right now.
    #[must_use]
    pub fn available(&self) -> usize {
        self.lock().free
    }

    /// Pokes ever sent. One per release under contention, plus one per steal.
    #[doc(hidden)]
    #[must_use]
    pub fn pokes(&self) -> u64 {
        self.lock().pokes
    }

    /// Registry entries right now, live or dead, for the probes in `tests/engine.rs`.
    #[doc(hidden)]
    #[must_use]
    pub fn parked(&self) -> usize {
        self.lock().parked.len()
    }

    /// Take a free permit, or `None` — never waits and never parks.
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>) -> Option<ClonePermit> {
        self.take(None)
    }

    /// Take a free permit, consuming the `poke` that re-drove this try if there is one.
    /// `None` leaves the caller to [`park`](Self::park) once its lane is released.
    pub(super) fn take(self: &Arc<Self>, poke: Option<Poke>) -> Option<ClonePermit> {
        // Disarmed, not forgotten: its `Drop` must not decline what this consumes.
        let consumed = poke.map(|mut poke| poke.0.take()).is_some();
        self.settle(|s| {
            s.poked -= usize::from(consumed);
            (s.free > 0).then(|| {
                s.free -= 1;
                ClonePermit(Arc::clone(self))
            })
        })
    }

    /// Queue `driver` for a poke. Called when the deferral lands, with the lane already
    /// released — so a permit freed since the failed try is still seen: it left
    /// `poked < free`, and this rebalance pokes for it.
    ///
    /// A driver already parked keeps its one entry and its place: a re-dispatch that
    /// defers again while still queued (a `RootsChanged`, say) adds nothing.
    pub(super) fn park(self: &Arc<Self>, driver: &mpsc::UnboundedSender<Msg>) {
        self.settle(|s| {
            // Upgrading under the lock is safe: the temporary sender is never the
            // channel's last handle — the weak entry it came from still holds it — so
            // dropping it here cannot drop a stranded message.
            let queued = s
                .parked
                .iter()
                .any(|w| w.upgrade().is_some_and(|t| t.same_channel(driver)));
            if !queued {
                s.parked.push_back(driver.downgrade());
            }
        });
    }

    /// Apply `change` under the lock, poke parked drivers until every free permit has a
    /// poke in flight, and send those pokes once the lock is released.
    fn settle<T>(self: &Arc<Self>, change: impl FnOnce(&mut State) -> T) -> T {
        let mut woken = Vec::new();
        let out = {
            let mut s = self.lock();
            let out = change(&mut s);
            while s.poked < s.free
                && let Some(driver) = s.parked.pop_front()
            {
                s.poked += 1;
                s.pokes += 1;
                woken.push(driver);
            }
            out
        };
        // Built out here, never under the lock: a driver that is gone — un-upgradable,
        // or hands the poke back in the send error — drops it, which declines, and the
        // next parked driver is poked in its place.
        for weak in woken {
            let poke = Poke(Some(Arc::clone(self)));
            if let Some(driver) = weak.upgrade() {
                let _ = driver.send(Msg::PermitFreed(poke));
            }
        }
        out
    }

    /// Poison is survivable: every critical section is a few counter updates and a
    /// queue push or pop, so a panicking holder cannot have left the state half-built.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for ClonePermit {
    fn drop(&mut self) {
        self.0.settle(|s| s.free += 1);
    }
}

impl Drop for Poke {
    fn drop(&mut self) {
        if let Some(permits) = self.0.take() {
            permits.settle(|s| s.poked -= 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ClonePermits, Msg, Poke};
    use std::sync::Arc;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// A parked driver's mailbox: the sender kept alive, since the registry holds it weak.
    type Mailbox = (UnboundedSender<Msg>, UnboundedReceiver<Msg>);

    /// A bound of one, held, with `n` drivers parked behind it.
    fn contended(n: usize) -> (Arc<ClonePermits>, super::ClonePermit, Vec<Mailbox>) {
        let permits = Arc::new(ClonePermits::new(1));
        let held = permits.try_acquire().unwrap();
        let parked = (0..n)
            .map(|_| {
                let (tx, rx) = unbounded_channel();
                permits.park(&tx);
                (tx, rx)
            })
            .collect();
        (permits, held, parked)
    }

    fn poke((_, rx): &mut Mailbox) -> Option<Poke> {
        match rx.try_recv() {
            Ok(Msg::PermitFreed(poke)) => Some(poke),
            _ => None,
        }
    }

    /// A release wakes one driver, the oldest — not every waiter.
    #[test]
    fn a_release_pokes_exactly_one_parked_driver() {
        let (permits, held, mut parked) = contended(3);
        assert!(permits.try_acquire().is_none());
        drop(held);
        // Held, not dropped as read: a dropped poke declines and wakes the next driver.
        let pokes: Vec<_> = parked.iter_mut().map(poke).collect();
        assert_eq!(
            pokes.iter().map(Option::is_some).collect::<Vec<_>>(),
            [true, false, false]
        );
        assert_eq!(permits.pokes(), 1);
    }

    /// A poke that reaches nobody — the driver is gone, or it declined — passes on to
    /// the next parked driver.
    #[test]
    fn a_poke_nobody_takes_passes_to_the_next_parked_driver() {
        let (permits, held, mut parked) = contended(3);
        parked.remove(0);
        drop(held);
        assert!(poke(&mut parked[1]).is_none());
        drop(poke(&mut parked[0]));
        assert!(poke(&mut parked[1]).is_some());
        assert_eq!(permits.pokes(), 3);
    }

    /// The registry holds a parked driver weakly: once the driver drops its sender, the
    /// mailbox is closed, so nothing stranded in it can outlive the driver undeclined.
    #[test]
    fn a_parked_entry_does_not_keep_the_mailbox_alive() {
        let (_permits, _held, mut parked) = contended(1);
        let (tx, mut rx) = parked.remove(0);
        drop(tx);
        assert_eq!(rx.try_recv().err(), Some(TryRecvError::Disconnected));
    }

    /// A driver parked twice holds one entry: a release pokes it once, and the next
    /// release finds nobody to poke.
    #[test]
    fn a_driver_parked_twice_is_poked_once() {
        let (permits, held, mut parked) = contended(1);
        permits.park(&parked[0].0);
        assert_eq!(permits.parked(), 1);
        drop(held);
        let permit = permits.take(poke(&mut parked[0])).unwrap();
        drop(permit);
        assert!(poke(&mut parked[0]).is_none());
        assert_eq!((permits.pokes(), permits.parked()), (1, 0));
    }

    /// A poke consumed by its try takes the permit and wakes nobody else; the next
    /// driver is poked when that permit comes back.
    #[test]
    fn a_consumed_poke_takes_the_permit_and_wakes_nobody_else() {
        let (permits, held, mut parked) = contended(2);
        drop(held);
        let permit = permits.take(poke(&mut parked[0])).unwrap();
        assert!(poke(&mut parked[1]).is_none());
        drop(permit);
        assert!(poke(&mut parked[1]).is_some());
        assert_eq!(permits.available(), 1);
    }

    /// A fresh try may steal the permit a poke was meant for; the robbed driver's try
    /// then fails, consuming its poke, and once it parks again the stolen permit's
    /// release pokes it.
    #[test]
    fn a_stolen_permit_costs_the_poked_driver_one_re_park() {
        let (permits, held, mut parked) = contended(1);
        drop(held);
        let stolen = permits.try_acquire().unwrap();
        assert!(permits.take(poke(&mut parked[0])).is_none());
        assert_eq!((permits.pokes(), permits.available()), (1, 0));
        permits.park(&parked[0].0);
        drop(stolen);
        assert!(poke(&mut parked[0]).is_some());
        assert_eq!(permits.pokes(), 2);
    }
}
