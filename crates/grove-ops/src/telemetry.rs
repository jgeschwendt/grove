//! `OTel` export for grove-ops. Off unless `GROVE_TELEMETRY` is set **and**
//! `GROVE_OTLP_TRACES_ENDPOINT` names a collector — there is no default destination,
//! so opting in never exports repo identity to a host nobody named. When on, an op
//! runs in a span optionally parented to a W3C `traceparent` its caller supplies.
//!
//! The transport is the blocking one (simple span exporter + reqwest-blocking) this
//! crate needed when it was a synchronous process of its own. [`redact_url`] and the
//! attribute discipline are transport-independent and stand as written.
//!
//! **Nothing calls [`init`] today.** No binary in this workspace wires this module
//! up, so it exports no spans as shipped; the daemon's observability is the `tracing`
//! subscriber `grove serve` installs (stderr plus the log ring `GET /api/events`
//! streams). Wiring it up means calling [`init`] and holding the [`Telemetry`] handle
//! for the process's life — and, in the daemon, replacing the blocking exporter with
//! a batch one so an export cannot sit on a runtime thread. See
//! `docs/architecture.md` § Telemetry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opentelemetry::{
    Context, KeyValue, global,
    propagation::{Extractor, TextMapPropagator},
    trace::{SpanKind, Status, TraceContextExt, Tracer},
};
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{Resource, propagation::TraceContextPropagator, trace::SdkTracerProvider};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Provider handle. Flushes + shuts down on drop so a one-shot process exports
/// its spans before exit. `None` when telemetry is off.
pub struct Telemetry(Option<SdkTracerProvider>);

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(provider) = &self.0 {
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        }
    }
}

/// Initialise `OTel` if `GROVE_TELEMETRY` opts in; otherwise a no-op handle.
pub fn init() -> Telemetry {
    if !enabled_env() {
        return Telemetry(None);
    }

    match build() {
        Ok(provider) => {
            global::set_tracer_provider(provider.clone());
            ENABLED.store(true, Ordering::Relaxed);
            Telemetry(Some(provider))
        }
        Err(_) => Telemetry(None),
    }
}

fn enabled_env() -> bool {
    matches!(
        std::env::var("GROVE_TELEMETRY").as_deref(),
        Ok("1" | "true" | "yes")
    )
}

fn build() -> anyhow::Result<SdkTracerProvider> {
    // No default destination. Spans carry `grove.slug` and a redacted `grove.url`,
    // so a built-in fallback host would make opting telemetry in an opt-in to
    // exporting repo identity somewhere the operator never named. Unset is a
    // no-op handle, not an egress.
    let endpoint = std::env::var("GROVE_OTLP_TRACES_ENDPOINT")
        .map_err(|_| anyhow::anyhow!("GROVE_OTLP_TRACES_ENDPOINT is unset"))?;

    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        // Bound the blocking export so a slow/hung OTLP endpoint can't stall an op.
        .with_timeout(Duration::from_secs(5))
        .build()?;

    let provider = SdkTracerProvider::builder()
        // 0.32: builder takes the exporter directly (no Box, no public
        // SimpleSpanProcessor::new); with_simple_exporter wraps it synchronously.
        .with_simple_exporter(exporter)
        .with_resource(Resource::builder().with_service_name("grove-ops").build())
        .build();

    Ok(provider)
}

/// Run `f` inside a `grove-ops.<op>` span parented to `traceparent` (when present),
/// marking the span failed if `f` returns `Err` so a failed op doesn't look
/// successful in the trace. No-op — just calls `f` — when telemetry is off.
pub fn in_span<T, E: std::fmt::Display>(
    op: &str,
    traceparent: Option<&str>,
    f: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    if !ENABLED.load(Ordering::Relaxed) {
        return f();
    }

    let parent = extract_parent(traceparent);
    let tracer = global::tracer("grove-ops");
    let span = tracer
        .span_builder(format!("grove-ops.{op}"))
        .with_kind(SpanKind::Server)
        .start_with_context(&tracer, &parent);

    let _guard = parent.with_span(span).attach();
    let result = f();
    if let Err(e) = &result {
        Context::current()
            .span()
            .set_status(Status::error(e.to_string()));
    }
    result
}

/// Set an attribute on the currently-active `grove-ops` span. No-op when
/// telemetry is off. High-cardinality values (slugs, URLs) belong here on spans,
/// never as metric labels. **Redact secrets first** — a clone URL can carry an
/// embedded token; pass it through [`redact_url`] before setting `grove.url`.
pub fn set_attr(key: &'static str, value: impl Into<opentelemetry::Value>) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    Context::current()
        .span()
        .set_attribute(KeyValue::new(key, value));
}

/// Strip any `userinfo@` (embedded credentials) from a URL before it becomes a
/// span attribute or log field. A private-repo clone URL like
/// `https://x-access-token:SECRET@github.com/o/r.git` — the standard agent-driven
/// pattern — must never ship its token to the OTLP backend. Returns the URL with
/// the `user[:pass]@` segment removed; input without a `scheme://authority` shape
/// (scp-style `git@host:o/r`, which carries no password) is returned unchanged.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after = &url[scheme_end + 3..];
    // The authority ends at the first `/`, `?`, or `#` (RFC 3986) — bounding only
    // at `/` would mis-classify a query/fragment as part of the host.
    let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (authority, rest) = after.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((_userinfo, host)) => format!("{}://{host}{rest}", &url[..scheme_end]),
        None => url.to_string(),
    }
}

fn extract_parent(traceparent: Option<&str>) -> Context {
    match traceparent {
        None => Context::new(),
        Some(tp) => TraceContextPropagator::new().extract(&Traceparent(tp)),
    }
}

struct Traceparent<'a>(&'a str);

impl Extractor for Traceparent<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        if key.eq_ignore_ascii_case("traceparent") {
            Some(self.0)
        } else {
            None
        }
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent"]
    }
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn redact_url_strips_embedded_credentials() {
        assert_eq!(
            redact_url("https://x-access-token:TOKEN@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(redact_url("https://user@host/o/r"), "https://host/o/r");
        assert_eq!(redact_url("ssh://git@host:22/o/r"), "ssh://host:22/o/r");
        // No userinfo, no scheme, or scp-style: returned unchanged.
        assert_eq!(
            redact_url("https://github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            redact_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
        assert_eq!(redact_url("https://host"), "https://host");
        // A `?`/`#` before any `/` ends the authority — the host isn't mangled and a
        // query value isn't promoted to be the host.
        assert_eq!(
            redact_url("https://host.com?next=user@example.com"),
            "https://host.com?next=user@example.com"
        );
        assert_eq!(
            redact_url("https://user:secret@host?token=abc"),
            "https://host?token=abc"
        );
    }
}
