//! grove-daemon — the resident half: the axum HTTP API, boot state, the readiness
//! and mutation guards, per-root lanes, the convergence engine, the manifest
//! watcher and the event bus. Owns running things only; everything durable lives
//! in `manifest.toml` and git.
//!
//! ## What is resident, and why
//!
//! - [`lane`] — one actor per active root, so every git write for that root is
//!   serialized (carried law 6). This is the invariant `grove-ops` was written
//!   against and does not itself enforce.
//! - [`engine`] — one driver per declared root: the status cache, one background
//!   slot with **reconcile > sync > fill** priority, level-triggered flags so a
//!   burst coalesces and never double-clones. No timers: the next event is the
//!   retry (carried law 8).
//! - [`engine::RootSet`] — declared slugs in, running engines out, on every
//!   roots-changed.
//! - [`watcher`] — discovery and announcement: `roots.adopt` (never a clone) then
//!   `list`, debounced, triggered by the HTTP nudge and optionally by the
//!   filesystem.
//! - [`events`] — the push channel the whole thing publishes onto.
//! - [`logs`] — a bounded ring of the daemon's own log lines, fed by a `tracing`
//!   layer the process installs, on a channel of its own beside the bus.
//! - [`stream`] — the read surface: one snapshot shape served as `GET /api/roots`
//!   and as the opening frame of the `GET /api/events` stream.
//!
//! Nothing here is a source of truth. Status is a cache, the pool count is a hint,
//! and a restarted daemon re-derives both from the filesystem (carried law 11).
//!
//! The shape a caller sees:
//!
//! ```no_run
//! # async fn wire() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use grove_daemon::{Config, Daemon};
//! use grove_ops::clock::SystemClock;
//!
//! let daemon = Daemon::bind(Config::from_env()?, Arc::new(SystemClock)).await?;
//! daemon.state().boot.mark_ready();
//! daemon.serve().await?;
//! # Ok(()) }
//! ```
//!
//! Binding, readiness and serving are three separate steps on purpose: a port-0
//! bind must report the address it got before anything can reach it, and readiness
//! is the *caller's* signal — `grove serve` gives it once the process is up, and a
//! test serves a deliberately `booting` or `degraded` daemon to exercise the gate.
//!
//! `serve` also starts the engine room — the watcher and the engine set — unless
//! the caller has claimed the reconcile mailbox with
//! [`Daemon::take_nudges`](app::Daemon::take_nudges), which means "I am driving
//! convergence, not you". The HTTP contract suite does exactly that.

pub mod app;
pub mod boot;
pub mod config;
pub mod engine;
mod error;
pub mod events;
pub mod guard;
pub mod lane;
pub mod logs;
pub mod reply;
pub mod routes;
pub mod stream;
mod wait;
pub mod watcher;

pub use app::{AppState, Daemon, ReconcileNudge, ShutdownTrigger};
pub use boot::BootState;
pub use config::{Config, DEFAULT_BIND, parse_bind};
pub use engine::{Engine, EngineError, RootSet, SyncInfo};
pub use error::Error;
pub use events::EventBus;
pub use lane::{LaneError, Lanes, Priority};
pub use logs::LogRing;
