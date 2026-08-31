//! The two guards every `/api` request passes: readiness, then mutation.
//!
//! Both are compensating controls for an unauthenticated API, and both retire only
//! when served-mode auth lands — together with the loopback bind gate they are the
//! whole of carried law 10.

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use grove_api::{ApiError, BootStatus, ErrorCode};

use crate::app::AppState;
use crate::reply::error_response;

/// Routes that answer while the daemon is draining or degraded.
///
/// Health, so the state stays *observable* — it is what the self-update gate reads
/// to decide a bundle went bad — and shutdown, so a late or repeated stop request
/// (and stopping an already-degraded daemon) is not itself 503'd.
// stele:landmark readiness-whitelist
const READINESS_WHITELIST: [&str; 2] = ["/api/health", "/api/daemon/shutdown"];

/// The **fixed** allowlist of `Origin` hosts a state-changing request may carry.
///
/// Fixed, never the request's own `Host` header, is what makes this a DNS-rebinding
/// defense rather than a decoration: a page served from `evil.example` still sends
/// `Origin: https://evil.example` after its name has been rebound to 127.0.0.1, so
/// the origin — which the attacker cannot forge from a browser — fails this list
/// while a `Host` check would pass.
// stele:landmark unauthenticated-api
const LOOPBACK_ORIGINS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// 503 every non-whitelisted `/api` route while the daemon is draining or degraded.
///
/// Note what is *not* gated: `booting`. v1's plug tested only `:stopping` and
/// `{:degraded,_}`, and its endpoint was listening before the post-boot ready signal
/// — so a request arriving in that window was served. Carried as written; in v2 the
/// window is a few microseconds wide, between `Daemon::bind` and `mark_ready`.
pub async fn readiness(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if !is_api(path) || READINESS_WHITELIST.contains(&path) {
        return next.run(request).await;
    }
    let message = match state.boot.status() {
        BootStatus::Stopping => "server is draining",
        BootStatus::Degraded { .. } => "server is degraded",
        BootStatus::Booting | BootStatus::Ready => return next.run(request).await,
    };
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ApiError::new(ErrorCode::Unavailable, message),
    )
}

/// Refuse a state-changing `/api` request that carries a non-loopback `Origin`.
///
/// Safe methods pass untouched. For the rest the rule is v1's, verbatim: allow when
/// there is **no `Origin`** — the CLI and every other non-browser client send none,
/// so dropping this allowance breaks every mutation grove itself performs — or when
/// the origin's host is one of [`LOOPBACK_ORIGINS`].
///
/// Residual, accepted until auth lands: another app already on a loopback port can
/// present a loopback origin. Closing that needs request authentication, which is
/// the work this guard stands in for.
// stele:landmark mutation-guard
pub async fn mutation(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if safe || !is_api(path) {
        return next.run(request).await;
    }
    let allowed = match request.headers().get(axum::http::header::ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .ok()
            .and_then(origin_host)
            .is_some_and(|host| LOOPBACK_ORIGINS.contains(&host)),
    };
    if allowed {
        return next.run(request).await;
    }
    error_response(
        StatusCode::FORBIDDEN,
        ApiError::new(ErrorCode::Forbidden, "cross-origin request refused"),
    )
}

/// Whether a path is on the JSON API — the surface v1 put behind its `:api`
/// pipeline. Prefix-matched on the *segment*, so a hypothetical `/apidocs` is not
/// swept in by a bare `starts_with("/api")`.
fn is_api(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// The host of an `Origin` header value (`scheme://host[:port]`).
///
/// `None` for anything that is not that shape — `null` from a sandboxed frame, a
/// bare host, an authority carrying userinfo — and a `None` host is refused, so the
/// unparseable cases fail closed rather than matching an empty string against the
/// allowlist.
fn origin_host(origin: &str) -> Option<&str> {
    let authority = origin.split_once("://")?.1;
    let authority = authority.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        // An IPv6 literal: the host is inside the brackets, and the `:`s within it
        // are not the port separator.
        return rest.split_once(']').map(|(host, _)| host);
    }
    let host = authority.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::{LOOPBACK_ORIGINS, READINESS_WHITELIST, is_api, origin_host};

    #[test]
    fn the_api_surface_is_matched_by_segment() {
        assert!(is_api("/api") && is_api("/api/health") && is_api("/api/roots/remove"));
        assert!(!is_api("/apidocs"), "a sibling path is not the API");
        assert!(!is_api("/") && !is_api("/health"));
    }

    #[test]
    fn origin_host_reads_the_shapes_a_browser_sends() {
        assert_eq!(origin_host("http://127.0.0.1:7777"), Some("127.0.0.1"));
        assert_eq!(origin_host("https://localhost"), Some("localhost"));
        assert_eq!(origin_host("http://[::1]:7777"), Some("::1"));
        assert_eq!(
            origin_host("https://evil.example:443"),
            Some("evil.example")
        );
        // A rebound attacker origin still names the attacker's host.
        assert_eq!(
            origin_host("http://evil.example:7777"),
            Some("evil.example")
        );
    }

    /// Everything unparseable fails closed: no host means no match, and no match
    /// means refused.
    #[test]
    fn an_unparseable_origin_has_no_host() {
        assert_eq!(origin_host("null"), None);
        assert_eq!(origin_host("127.0.0.1:7777"), None, "no scheme");
        assert_eq!(origin_host("http://"), None, "empty authority");
        assert_eq!(
            origin_host("http://localhost@evil.example"),
            None,
            "userinfo could disguise the real host"
        );
        for host in [origin_host("null"), origin_host("http://")] {
            assert!(!host.is_some_and(|h| LOOPBACK_ORIGINS.contains(&h)));
        }
    }

    /// The two lists are the guards' whole policy, so they are pinned as literals
    /// rather than only exercised through a request.
    #[test]
    fn the_allowlists_are_exactly_v1s() {
        assert_eq!(LOOPBACK_ORIGINS, ["localhost", "127.0.0.1", "::1"]);
        assert_eq!(
            READINESS_WHITELIST,
            ["/api/health", "/api/daemon/shutdown"],
            "widening this list exposes a route on a draining server"
        );
    }
}
