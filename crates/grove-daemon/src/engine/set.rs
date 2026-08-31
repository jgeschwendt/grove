//! The engine set: declared roots in, running engines out.
//!
//! One task owns the map, so "start the declared-minus-running, stop the
//! running-minus-declared" is an ordinary set difference rather than a concurrency
//! problem. That single ownership is also what closes v1's undeclare→redeclare race
//! without any of v1's machinery: a slug stopped in one pass is already out of the
//! map before the next pass reads it, because both passes are the same task.
//!
//! The set is driven by the event bus, never by the watcher directly — anything that
//! publishes `roots_changed` updates the engines for free, and the watcher stays
//! single-purpose (discover and announce; the engines realize).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grove_api::events::Event;
use grove_api::routes::RootStatusEntry;
use grove_api::status::RootStatus;
use tokio::sync::{broadcast, mpsc, oneshot};

use super::{Deps, Engine};

/// How long a status read waits on one engine before reporting `unavailable`.
///
/// A driver only ever awaits its mailbox, so this should never fire — every
/// grove-ops call leaves the loop as a task. It is the belt to that braces: v1's
/// doctor caught a wedged engine read and omitted the root, and a doctor that hangs
/// forever on one bad engine is worse than one that says so.
const STATUS_BUDGET: Duration = Duration::from_secs(5);

/// A handle on the engine set.
#[derive(Clone, Debug)]
pub struct RootSet {
    tx: mpsc::UnboundedSender<Msg>,
}

enum Msg {
    /// Re-read the manifest and reconcile against it — the cold boot, and the
    /// recovery path when the bus tells us we missed events.
    Resync,
    Engine(String, oneshot::Sender<Option<Engine>>),
    Slugs(oneshot::Sender<Vec<String>>),
    /// Every running engine's status, or just one root's.
    Statuses(Option<String>, oneshot::Sender<Vec<RootStatusEntry>>),
    Shutdown,
}

impl RootSet {
    /// Start the set, subscribing to `deps.events` and cold-booting from the
    /// manifest.
    ///
    /// The cold boot reads the declared set itself rather than waiting for the first
    /// broadcast, for v1's reason: the boot-time announcement can be published before
    /// this task subscribes, and a set that waited for it would sit empty until the
    /// next manifest change.
    #[must_use]
    pub fn start(deps: Deps) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let events = deps.events.subscribe();
        tokio::spawn(
            Set {
                deps,
                engines: HashMap::new(),
            }
            .run(rx, events),
        );
        let set = Self { tx };
        set.resync();
        set
    }

    /// Re-read the manifest and reconcile the engine set against it.
    pub fn resync(&self) {
        let _ = self.tx.send(Msg::Resync);
    }

    /// The engine driving `slug`, if one is running.
    pub async fn engine(&self, slug: &str) -> Option<Engine> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::Engine(slug.to_owned(), tx)).ok()?;
        rx.await.ok().flatten()
    }

    /// The slugs with a running engine.
    pub async fn slugs(&self) -> Vec<String> {
        self.ask(Msg::Slugs).await.unwrap_or_default()
    }

    /// Engine statuses for `POST /api/doctor` — scoped to `slug` when the request
    /// named one, every running engine otherwise.
    ///
    /// This is the only place `cloning` and `degraded` are observable: neither is
    /// derivable from disk, so a root mid-clone or stopped on a terminal fault is
    /// invisible to every other check doctor runs.
    pub async fn statuses(&self, slug: Option<&str>) -> Vec<RootStatusEntry> {
        let slug = slug.map(ToOwned::to_owned);
        self.ask(|reply| Msg::Statuses(slug, reply))
            .await
            .unwrap_or_default()
    }

    /// Stop every engine and the set itself.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
    }

    async fn ask<T>(&self, msg: impl FnOnce(oneshot::Sender<T>) -> Msg) -> Option<T> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(msg(tx)).ok()?;
        rx.await.ok()
    }
}

struct Set {
    deps: Deps,
    engines: HashMap<String, Engine>,
}

impl Set {
    async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Msg>,
        mut events: broadcast::Receiver<Event>,
    ) {
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(Msg::Resync) => {
                        // Skipped, not reconciled-against-nothing, when the read
                        // failed — see `read_declared`.
                        if let Some(declared) = read_declared(&self.deps.home).await {
                            self.reconcile(declared);
                        }
                    }
                    Some(Msg::Engine(slug, reply)) => {
                        let _ = reply.send(self.engines.get(&slug).cloned());
                    }
                    Some(Msg::Slugs(reply)) => {
                        let mut slugs: Vec<String> = self.engines.keys().cloned().collect();
                        slugs.sort();
                        let _ = reply.send(slugs);
                    }
                    Some(Msg::Statuses(slug, reply)) => self.spawn_statuses(slug.as_deref(), reply),
                    Some(Msg::Shutdown) | None => break,
                },
                event = events.recv() => match event {
                    Ok(Event::RootsChanged { roots }) => self.reconcile(roots),
                    Ok(_) => {}
                    // We missed events. Every one of them was level-triggered, so the
                    // recovery is not to replay them but to re-read the world — which
                    // is exactly what a `roots_changed` would have asked for.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "engine set lagged the event bus; re-reading");
                        if let Some(declared) = read_declared(&self.deps.home).await {
                            self.reconcile(declared);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        for engine in self.engines.values() {
            engine.stop();
        }
    }

    /// Start what is declared and not running, stop what is running and not
    /// declared, and tell the survivors the manifest moved.
    fn reconcile(&mut self, declared: Vec<String>) {
        self.engines.retain(|slug, engine| {
            let keep = declared.iter().any(|d| d == slug);
            if !keep {
                engine.stop();
            }
            keep
        });
        for slug in declared {
            // Already running: it re-derives and re-dispatches a reconcile — the op
            // is idempotent and worktrees may have changed out of band, so this
            // fires even for a realized root. New: it starts with a reconcile
            // already pending, so it needs no separate nudge.
            if let Some(engine) = self.engines.get(&slug) {
                engine.roots_changed();
            } else {
                let engine = Engine::start(self.deps.clone(), slug.clone());
                self.engines.insert(slug, engine);
            }
        }
        tracing::debug!(engines = self.engines.len(), "engine set reconciled");
    }

    /// Read every engine's status off the set's own task, so one slow engine does not
    /// stall the set's mailbox behind it.
    fn spawn_statuses(&self, slug: Option<&str>, reply: oneshot::Sender<Vec<RootStatusEntry>>) {
        let engines: Vec<Engine> = match slug {
            Some(slug) => self.engines.get(slug).cloned().into_iter().collect(),
            None => self.engines.values().cloned().collect(),
        };
        let clock = Arc::clone(&self.deps.clock);
        tokio::spawn(async move {
            let budget = clock.deadline(STATUS_BUDGET);
            let mut out = Vec::with_capacity(engines.len());
            for engine in engines {
                // On the injected clock, not tokio's — see `wait::within`.
                let status = crate::wait::within(budget, &*clock, engine.status())
                    .await
                    .unwrap_or(Ok(RootStatus::Unavailable))
                    .unwrap_or(RootStatus::Unavailable);
                out.push(RootStatusEntry {
                    slug: engine.slug().to_owned(),
                    status,
                });
            }
            // Degraded first, then by slug: the recovery channel for a wedged clone
            // must be the first thing an operator reads, not buried alphabetically.
            out.sort_by(|a, b| {
                let rank = |s: RootStatus| u8::from(s != RootStatus::Degraded);
                rank(a.status)
                    .cmp(&rank(b.status))
                    .then_with(|| a.slug.cmp(&b.slug))
            });
            let _ = reply.send(out);
        });
    }
}

/// The declared slugs, read off the manifest on a blocking thread — or `None` when
/// the read did not happen.
///
/// **`None` is not an empty manifest, and the two must never collapse.** `reconcile`
/// treats "not in the declared list" as "stop this engine", so folding a failed read
/// into `Vec::new()` executes "I could not read desired state" as "there is no
/// desired state": one malformed hand-edit or transient EIO tears down the driver for
/// every root in the daemon, taking every `cloning` and `degraded` fact with it —
/// the two statuses no later disk read can recover, and the only reason an engine is
/// resident at all. Push-based convergence (carried law 8) means nothing re-drives
/// it afterwards either.
///
/// This is [`crate::watcher::spawn_adopt`]'s rule, applied where the consequence is
/// worse: announcing a set we could not read would be bad; *acting* on one is
/// destructive.
async fn read_declared(home: &Path) -> Option<Vec<String>> {
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || grove_ops::roots::list(&home))
        .await
        .map_err(|e| tracing::warn!(error = %e, "reading declared roots panicked"))
        .ok()?
        .map_err(
            |e| tracing::warn!(error = %e, "reading declared roots failed; keeping every engine"),
        )
        .ok()
        .map(|roots| roots.into_iter().map(|root| root.slug).collect())
}
