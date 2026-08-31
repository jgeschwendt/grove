//! The log ring — v1's `LogBuffer`, as a `tracing` layer.
//!
//! A bounded ring of the last [`CAPACITY`] lines, plus a broadcast of each line as it
//! lands. `GET /api/events` carries both: the ring in the snapshot it opens with, the
//! broadcast as `log` frames after that. Nothing else reads it; the daemon's own
//! diagnostics still go to stderr through whatever subscriber the process installed.
//!
//! ## Why a channel of its own, beside the event bus
//!
//! The [`EventBus`](crate::EventBus) carries level-triggered state: "something
//! changed, re-read it". Losing one of those costs a moment of staleness. A log line
//! is the opposite — it *is* the information, and a dropped one is gone. Sharing a
//! buffer would let a chatty minute evict pending state events and leave a UI stale
//! for the wrong reason, so the two are separate channels with separate capacities
//! and separate lag handling: the bus's overflow triggers a resync, this one's is
//! reported as a count of lines nobody will see.
//!
//! ## What a line may carry
//!
//! A `tracing` event carries whatever fields its macro was given, and a stream that
//! forwarded all of them would publish whatever a future `warn!` happens to attach.
//! So the layer records a fixed shape — level, target, message — plus only the
//! [`FIELDS`] grove itself keys on, and renders every value through [`redact`], which
//! strips URL userinfo. Anything else is dropped at the layer, before it can reach a
//! subscriber.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use grove_api::events::{LogField, LogLevel, LogLine};
use grove_ops::clock::Clock;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// How many lines the ring holds. v1's `LogBuffer` size, carried.
pub const CAPACITY: usize = 500;

/// How many lines a live subscriber may fall behind before it is told how many it
/// lost. Smaller than the ring: a subscriber this far behind is not going to catch
/// up, and the ring is what a reconnect reads anyway.
pub const CHANNEL_DEPTH: usize = 256;

/// The default capture level — **`debug` and `trace` are dropped**. The ring is a UI
/// affordance, and one debug-level module can produce thousands of lines a second;
/// what reaches an operator's screen should be what grove chose to say out loud.
/// `GROVE_LOG_RING` raises or lowers it.
pub const DEFAULT_LEVEL: Level = Level::INFO;

/// The structured fields a line may carry, alphabetically. Everything grove's own
/// `tracing` macros key on, and nothing else — see the module doc.
pub const FIELDS: &[&str] = &[
    "bind", "code", "count", "engines", "error", "from", "home", "kind", "missed", "name",
    "outcome", "path", "pool", "pruned", "reason", "slug", "status", "target", "to", "version",
];

/// The bounded ring, and the broadcast beside it.
pub struct LogRing {
    clock: Arc<dyn Clock>,
    level: Level,
    lines: Mutex<VecDeque<LogLine>>,
    tx: broadcast::Sender<LogLine>,
}

impl std::fmt::Debug for LogRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogRing")
            .field("level", &self.level)
            .field("held", &self.lines().len())
            .finish_non_exhaustive()
    }
}

impl LogRing {
    /// A ring capturing at `level`, stamping through `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, level: Level) -> Self {
        Self {
            clock,
            level,
            lines: Mutex::new(VecDeque::with_capacity(CAPACITY)),
            tx: broadcast::channel(CHANNEL_DEPTH).0,
        }
    }

    /// The level at or above which lines are captured.
    #[must_use]
    pub const fn level(&self) -> Level {
        self.level
    }

    /// Append one line, evicting the oldest once full, and publish it.
    ///
    /// Public so the SSE seam can be tested without installing a global subscriber:
    /// a process has exactly one, and a test that needed it could not run beside its
    /// neighbours.
    pub fn record(&self, line: LogLine) {
        {
            let mut lines = self.lines();
            if lines.len() == CAPACITY {
                lines.pop_front();
            }
            lines.push_back(line.clone());
        }
        // Errs only when nobody is attached, which is the daemon's ordinary state.
        let _ = self.tx.send(line);
    }

    /// The ring's contents, oldest first — what a snapshot carries.
    #[must_use]
    pub fn recent(&self) -> Vec<LogLine> {
        self.lines().iter().cloned().collect()
    }

    /// Every line recorded from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }

    /// The `tracing` layer that feeds this ring. Install it beside whatever else the
    /// process logs through; it writes nowhere else.
    #[must_use]
    pub fn layer(self: &Arc<Self>) -> LogLayer {
        LogLayer {
            ring: Arc::clone(self),
        }
    }

    /// Build a line from a `tracing` event's parts. Separated from the layer so the
    /// whitelist and the redaction are testable without a subscriber.
    fn line(&self, metadata: &Metadata<'_>, message: String, fields: Vec<LogField>) -> LogLine {
        LogLine {
            at_ms: self.now_ms(),
            level: level_of(*metadata.level()),
            target: metadata.target().to_owned(),
            message,
            fields,
        }
    }

    /// Milliseconds since the Unix epoch, read through the clock seam — a *label* on
    /// a line, never a budget (see `Clock::wall`). `pub(crate)` so the stream's own
    /// synthetic lines are stamped the same way rather than re-deriving the epoch
    /// arithmetic beside it.
    pub(crate) fn now_ms(&self) -> u64 {
        self.clock
            .wall()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    /// A poisoned ring is not a reason to stop serving: the guarded value is a deque
    /// of owned lines, so a panicking holder cannot have left it half-built.
    fn lines(&self) -> MutexGuard<'_, VecDeque<LogLine>> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The `tracing` layer half of a [`LogRing`].
#[derive(Clone, Debug)]
pub struct LogLayer {
    ring: Arc<LogRing>,
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        // `Level`'s ordering is verbosity: TRACE is the greatest, ERROR the least.
        if *metadata.level() > self.ring.level {
            return;
        }
        let mut collector = Collector::default();
        event.record(&mut collector);
        self.ring.record(
            self.ring
                .line(metadata, collector.message, collector.fields),
        );
    }

    /// Tells the subscriber not to evaluate callsites this layer would drop anyway.
    /// A hint, not a filter: a sibling layer wanting `debug` still gets it, because
    /// the composed hint is the most verbose of the layers'.
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::from_level(self.ring.level))
    }
}

/// Pulls the message and the whitelisted fields out of one event.
#[derive(Default)]
struct Collector {
    message: String,
    fields: Vec<LogField>,
}

impl Collector {
    fn push(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = redact(value);
        } else if FIELDS.contains(&field.name()) {
            self.fields.push(LogField {
                name: field.name().to_owned(),
                value: redact(value),
            });
        }
    }
}

impl Visit for Collector {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.push(field, &format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.push(field, &value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.push(field, &value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.push(field, &value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.push(field, &value.to_string());
    }
}

/// Strip URL userinfo from a value on its way onto the wire.
///
/// A clone URL is the one piece of grove's own data that can carry a secret
/// (`https://user:token@host/…`), and it reaches a log line through git's stderr as
/// often as through a field grove wrote deliberately. v1 redacted `grove.url` on its
/// spans; this is the same rule applied where every value passes.
#[must_use]
pub fn redact(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(scheme) = rest.find("://") {
        let after = &rest[scheme + 3..];
        // Userinfo ends at the first `@` before the authority's own delimiters.
        let authority = after
            .find(['/', '?', '#', ' '])
            .map_or(after, |end| &after[..end]);
        out.push_str(&rest[..scheme + 3]);
        if let Some(at) = authority.find('@') {
            out.push_str("***@");
            rest = &after[at + 1..];
        } else {
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The wire spelling of a `tracing` level.
const fn level_of(level: Level) -> LogLevel {
    match level {
        Level::ERROR => LogLevel::Error,
        Level::WARN => LogLevel::Warn,
        Level::INFO => LogLevel::Info,
        Level::DEBUG => LogLevel::Debug,
        _ => LogLevel::Trace,
    }
}

/// Parse a level name for `GROVE_LOG_RING`; anything unrecognized takes `default`
/// rather than failing a boot over a log setting.
#[must_use]
pub fn parse_level(raw: Option<&str>, default: Level) -> Level {
    match raw.map(|raw| raw.trim().to_ascii_lowercase()).as_deref() {
        Some("error") => Level::ERROR,
        Some("warn") => Level::WARN,
        Some("info") => Level::INFO,
        Some("debug") => Level::DEBUG,
        Some("trace") => Level::TRACE,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, DEFAULT_LEVEL, LogRing, parse_level, redact};
    use grove_api::events::LogLevel;
    use grove_ops::clock::{TEST_EPOCH_SECS, TestClock};
    use std::sync::Arc;
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;

    fn ring(level: Level) -> Arc<LogRing> {
        Arc::new(LogRing::new(Arc::new(TestClock::new()), level))
    }

    /// The layer's whole contract in one pass: the message, the whitelisted field,
    /// the level, and the timestamp read through the clock seam.
    #[test]
    fn a_captured_event_becomes_a_line() {
        let ring = ring(DEFAULT_LEVEL);
        let subscriber = tracing_subscriber::registry().with(ring.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(slug = "o/r", "engine reconciled");
        });

        let lines = ring.recent();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].message, "engine reconciled");
        assert_eq!(lines[0].level, LogLevel::Info);
        assert_eq!(lines[0].fields.len(), 1);
        assert_eq!(lines[0].fields[0].name, "slug");
        assert_eq!(lines[0].fields[0].value, "o/r");
        assert_eq!(lines[0].at_ms, TEST_EPOCH_SECS * 1000);
        assert!(lines[0].target.starts_with("grove_daemon"));
    }

    /// The whitelist, from both directions: a field grove keys on travels, and one it
    /// does not is dropped at the layer rather than published.
    #[test]
    fn an_unlisted_field_never_reaches_a_line() {
        let ring = ring(DEFAULT_LEVEL);
        let subscriber = tracing_subscriber::registry().with(ring.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(slug = "o/r", authorization = "hunter2", "careful");
        });

        let fields = &ring.recent()[0].fields;
        assert_eq!(fields.len(), 1, "{fields:?}");
        assert_eq!(fields[0].name, "slug");
    }

    /// Debug is dropped by default — the ring is what an operator reads, not a
    /// firehose — and raising the level turns it back on.
    #[test]
    fn debug_is_dropped_at_the_default_level_and_kept_when_asked_for() {
        for (level, want) in [(DEFAULT_LEVEL, 0), (Level::DEBUG, 1)] {
            let ring = ring(level);
            let subscriber = tracing_subscriber::registry().with(ring.layer());
            tracing::subscriber::with_default(subscriber, || {
                tracing::debug!("chatter");
            });
            assert_eq!(ring.recent().len(), want, "at {level}");
        }
    }

    /// A URL's userinfo never reaches the wire — in a field or in the message.
    #[test]
    fn userinfo_is_redacted_wherever_it_appears() {
        assert_eq!(
            redact("clone https://alice:t0ken@github.com/o/r.git failed"),
            "clone https://***@github.com/o/r.git failed"
        );
        assert_eq!(redact("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(redact("https://github.com/o/r"), "https://github.com/o/r");
        assert_eq!(
            redact("a https://u:p@x/1 and b https://v:q@y/2"),
            "a https://***@x/1 and b https://***@y/2"
        );

        let ring = ring(DEFAULT_LEVEL);
        let subscriber = tracing_subscriber::registry().with(ring.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(error = "fatal: https://u:p@host/o/r.git", "clone failed");
        });
        assert_eq!(
            ring.recent()[0].fields[0].value,
            "fatal: https://***@host/o/r.git"
        );
    }

    /// Bounded, and it drops the *oldest*: a ring that shed new lines would show an
    /// operator the start of an incident and none of it.
    #[test]
    fn the_ring_holds_the_newest_lines_only() {
        let ring = ring(DEFAULT_LEVEL);
        let subscriber = tracing_subscriber::registry().with(ring.layer());
        tracing::subscriber::with_default(subscriber, || {
            for n in 0..CAPACITY + 10 {
                tracing::info!(count = n, "line");
            }
        });

        let lines = ring.recent();
        assert_eq!(lines.len(), CAPACITY);
        assert_eq!(
            lines[0].fields[0].value, "10",
            "the oldest ten were evicted"
        );
        assert_eq!(
            lines[CAPACITY - 1].fields[0].value,
            (CAPACITY + 9).to_string()
        );
    }

    /// Every recorded line also reaches a live subscriber — the SSE stream's half.
    #[tokio::test]
    async fn a_subscriber_sees_each_line_as_it_lands() {
        let ring = ring(DEFAULT_LEVEL);
        let mut rx = ring.subscribe();
        let subscriber = tracing_subscriber::registry().with(ring.layer());
        tracing::subscriber::with_default(subscriber, || tracing::info!("hello"));

        assert_eq!(rx.recv().await.unwrap().message, "hello");
    }

    #[test]
    fn the_ring_level_reads_the_usual_names_and_ignores_nonsense() {
        assert_eq!(parse_level(Some("debug"), DEFAULT_LEVEL), Level::DEBUG);
        assert_eq!(parse_level(Some(" WARN "), DEFAULT_LEVEL), Level::WARN);
        assert_eq!(parse_level(Some("loud"), DEFAULT_LEVEL), DEFAULT_LEVEL);
        assert_eq!(parse_level(None, DEFAULT_LEVEL), DEFAULT_LEVEL);
    }
}
