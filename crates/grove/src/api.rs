//! HTTP client for the daemon API.
//!
//! Decodes [`grove_api::Envelope`] — the contract crate both ends compile against,
//! so neither can drift from the other's spelling. Synchronous: the CLI is one-shot,
//! and none of these calls run inside `grove serve`'s runtime.
//!
//! Every budget spent here comes from [`crate::timeouts`]; none is written inline.

use std::net::SocketAddr;
use std::time::Duration;

use grove_api::routes::{
    DoctorData, DoctorRequest, HealthData, HealthStatus, Snapshot, SyncData, SyncRequest,
};
use grove_api::{Envelope, RemoveRootRequest, RemoveWorktreeRequest};
use serde::de::DeserializeOwned;

use crate::CliError;
use crate::timeouts;

/// `GROVE_BIND`'s default, spelled as the CLI writes it into a URL.
///
/// Derived from the daemon's own [`grove_daemon::DEFAULT_BIND`] rather than written
/// again: a second literal is a split brain waiting to happen — change the daemon's
/// default port and `on`/`off` (which already derive it) follow while `ok`/`doctor`/
/// `apply` keep talking to the old one, and a client that reads
/// connection-refused as `Offline` would then realize in-process beside a daemon that
/// is up. `SocketAddr`'s `Display` is exactly the `host:port` form a URL wants.
fn default_bind() -> String {
    grove_daemon::DEFAULT_BIND.to_string()
}

/// The three outcomes of probing the daemon, which the CLI's single-realizer decision
/// turns on (carried law 2).
///
/// Only a refused/failed *connection* means "no daemon". A timeout — or any other
/// post-connect error — means something is there and not answering in time, which must
/// NOT trigger in-process realization: two realizers racing is a dual clone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reachability {
    /// A daemon answered — any HTTP status, 5xx included. Delegate to it.
    Up,
    /// Connection refused / no listener. The CLI realizes in-process.
    Offline,
    /// Connected but timed out, or another transport error after connect. A daemon is
    /// up but busy: treat as online, never realize in-process.
    Busy,
}

/// A client bound to one daemon's base URL.
pub struct ApiClient {
    base: String,
    http: reqwest::blocking::Client,
    probe_timeout: Duration,
    /// The home this client was pointed at, when it was pointed at one — see
    /// [`ApiClient::reachable`]. `None` for the clients that address a *specific*
    /// daemon by socket (process custody, the self-update health gate), where the
    /// caller already knows whose daemon it is.
    home: Option<std::path::PathBuf>,
}

impl ApiClient {
    /// Target the daemon at `GROVE_BIND` (default `127.0.0.1:7777`), on behalf of
    /// `GROVE_HOME`.
    ///
    /// The raw bind goes into the URL unresolved: unlike the daemon — which decides
    /// what to *expose* and so refuses a hostname outright — the client only has to
    /// reach whatever is listening, and `GROVE_BIND=localhost:7777` must keep working.
    #[must_use]
    pub fn from_env() -> Self {
        let bind = std::env::var("GROVE_BIND").unwrap_or_else(|_| default_bind());
        Self::new(format!("http://{bind}"), timeouts::PROBE).for_home(grove_ops::home())
    }

    /// A client pointed at `addr` with an explicit probe budget — the constructor
    /// process custody and the dispatch tests use.
    #[must_use]
    pub fn at(addr: SocketAddr, probe_timeout: Duration) -> Self {
        Self::new(format!("http://{addr}"), probe_timeout)
    }

    /// Bind this client to the home its caller was pointed at, arming the identity
    /// check in [`Self::reachable`].
    #[must_use]
    pub fn for_home(mut self, home: impl Into<std::path::PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    fn new(base: String, probe_timeout: Duration) -> Self {
        Self {
            base,
            home: None,
            // No proxy: an `HTTP_PROXY` in the operator's environment must not sit
            // between the CLI and a daemon on its own loopback address — and would
            // also destroy the connect-vs-everything-else classification
            // [`Reachability`] is built on.
            //
            // `expect`, never `unwrap_or_default()`: the default client is
            // `Client::new()`, i.e. this same builder MINUS `no_proxy` — so the
            // fallback would silently hand back a proxy-honouring client whose
            // `is_connect()` verdicts no longer mean what `reachable()` reads them to
            // mean, and would panic on its own `expect` anyway when the real cause
            // (a TLS backend that will not initialise) recurred. Naming it is the
            // only honest option left.
            http: reqwest::blocking::Client::builder()
                .no_proxy()
                .build()
                .expect("grove: could not build the HTTP client"),
            probe_timeout,
        }
    }

    /// Classify the daemon at the base URL (see [`Reachability`]).
    ///
    /// The whole single-realizer gate rests on this one match arm: a connection-level
    /// failure is the *only* signal for "no daemon", and everything else — timeouts
    /// included — is a daemon that exists and is slow.
    ///
    /// **…and a daemon serving a different home is not this home's realizer.** The
    /// CLI resolves `GROVE_HOME` and `GROVE_BIND` as two unrelated facts, so a client
    /// scoped to one home routinely finds a *stranger's* daemon on the default bind —
    /// and used to hand it `roots/remove`, which deleted from the daemon's home and
    /// printed success. Nothing on the wire could catch it until `/api/health` began
    /// carrying the home. A daemon that answers for somewhere else is `Offline` for
    /// *us*: there is no realizer for this home, so the CLI realizes in-process,
    /// against the home the operator actually named. The dual-clone risk the tri-state
    /// exists to prevent needs two realizers over ONE home, which this is not.
    // stele:landmark reachability-tri-state
    #[must_use]
    pub fn reachable(&self) -> Reachability {
        match self.get("/api/health", self.probe_timeout) {
            Ok(response) => {
                if self.serves_another_home(response) {
                    Reachability::Offline
                } else {
                    Reachability::Up
                }
            }
            Err(e) if e.is_connect() => Reachability::Offline,
            Err(_) => Reachability::Busy,
        }
    }

    /// Does the daemon that just answered realize a *different* home than this client
    /// was pointed at?
    ///
    /// Only a decoded, ready payload naming a home that differs counts. Anything else
    /// — a non-2xx, an undecodable body, a client with no home of its own — is "not
    /// proven different", which keeps the answer `Up` and leaves the single-realizer
    /// gate exactly as it was.
    fn serves_another_home(&self, response: reqwest::blocking::Response) -> bool {
        let Some(mine) = self.home.as_deref() else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        let Ok(Envelope::Ok(data)) = response.json::<Envelope<HealthData>>() else {
            return false;
        };
        let theirs = std::path::PathBuf::from(&data.home);
        // Canonicalized, because `/tmp` and `/private/tmp` are the same home and a
        // string compare would call a daemon a stranger over a symlink.
        let resolve = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        resolve(&theirs) != resolve(mine)
    }

    /// `true` when `GET /api/health` returns 2xx within the probe budget — the
    /// readiness gate (the boot state is `ready`), never a merely-open socket.
    #[must_use]
    pub fn health_ok(&self) -> bool {
        self.get("/api/health", self.probe_timeout)
            .is_ok_and(|r| r.status().is_success())
    }

    /// The health payload when the daemon answers a ready 200; `None` on any
    /// non-2xx (a draining daemon's 503), transport error, or decode failure.
    #[must_use]
    pub fn health(&self) -> Option<HealthData> {
        let response = self.get("/api/health", timeouts::HEALTH).ok()?;
        if !response.status().is_success() {
            return None;
        }
        match response.json::<Envelope<HealthData>>().ok()? {
            Envelope::Ok(data) => Some(data),
            Envelope::Err(_) => None,
        }
    }

    /// The version the daemon reports, and only when it answers a *ready* 200.
    ///
    /// The self-update health gate compares this against the version it just flipped
    /// to: a 200 alone would let a stale process still bound to the port mask a failed
    /// update, and a draining old daemon's 503 must read as "not there yet".
    #[must_use]
    pub fn ready_version(&self) -> Option<String> {
        self.health().map(|data| data.version)
    }

    /// `true` when the listener speaks the grove envelope: `/api/health` returns a
    /// JSON object carrying a boolean `ok` at ANY status (a draining daemon 503s but
    /// still speaks it). This is what keeps `grove off` from aiming a shutdown POST
    /// at an unrelated service that happens to hold the bind.
    ///
    /// Deliberately shallower than a typed decode: identification must survive a
    /// version skew that a strict `Envelope<HealthData>` decode would reject, and
    /// `ok` is the one field every envelope on this surface has.
    #[must_use]
    pub fn speaks_grove(&self) -> bool {
        self.get("/api/health", self.probe_timeout)
            .ok()
            .and_then(|r| r.json::<serde_json::Value>().ok())
            .is_some_and(|v| v.get("ok").is_some_and(serde_json::Value::is_boolean))
    }

    /// Ask the daemon to drain and stop itself. `true` only when the request was
    /// accepted, so the caller knows whether it may wait for a clean exit or must
    /// escalate to a signal.
    #[must_use]
    pub fn request_shutdown(&self) -> bool {
        self.http
            .post(format!("{}/api/daemon/shutdown", self.base))
            .timeout(self.probe_timeout)
            .send()
            .is_ok_and(|r| r.status().is_success())
    }

    /// `grove ok`: ready → the status line; reachable but not ready → `Unhealthy`
    /// (exit 6); unreachable → `Daemon` (exit 4).
    ///
    /// The one route whose error envelope is *not* an `Api` failure: an unhealthy
    /// daemon answering correctly about being unhealthy is exactly what exit 6 names.
    pub fn ok(&self) -> Result<String, CliError> {
        let response = self.get("/api/health", timeouts::HEALTH).map_err(|e| {
            CliError::Daemon(format!(
                "cannot reach grove server at {} ({e}); is it running? try `grove on`",
                self.base
            ))
        })?;
        let healthy = response.status().is_success();
        let envelope: Envelope<HealthData> = response
            .json()
            .map_err(|e| CliError::Api(format!("invalid response from {}: {e}", self.base)))?;

        match envelope {
            Envelope::Ok(data) if healthy => Ok(format!(
                "grove server: {} (v{})",
                spelling(data.status),
                data.version
            )),
            // A success payload behind a failure status. The daemon never writes one,
            // so the status wins and the operator is told what it claimed to be.
            Envelope::Ok(data) => Err(CliError::Unhealthy(format!(
                "status {}",
                spelling(data.status)
            ))),
            Envelope::Err(e) => Err(CliError::Unhealthy(e.to_string())),
        }
    }

    /// Ask a running daemon to reconcile now — realize declared-but-missing roots and
    /// worktrees.
    ///
    /// The CLI calls this after declaring a change while a daemon is up
    /// (single-realizer), so realization is a reliable trigger rather than a hope that
    /// the fs watcher notices the manifest write. Returns as soon as the work is
    /// *scheduled*; a connection failure is `Daemon`, a reachable-but-refusing daemon
    /// (draining/degraded → non-2xx) is `Api`.
    pub fn reconcile(&self) -> Result<(), CliError> {
        let response = self
            .http
            .post(format!("{}/api/roots/reconcile", self.base))
            .timeout(timeouts::NUDGE)
            .send()
            .map_err(|e| self.unreachable(&e))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(CliError::Api(format!(
                "grove server refused to reconcile (HTTP {})",
                response.status()
            )))
        }
    }

    /// `POST /api/roots/sync {slug}` — ask a running daemon to fetch one root and
    /// fast-forward its `.trunk`.
    ///
    /// **Accept-only**, like the reconcile nudge and unlike doctor: the ack says the
    /// root's engine recorded the request, never that the fetch finished, so this
    /// spends [`timeouts::NUDGE`] rather than the ops budget. What the sync *did* is
    /// read afterwards — `grove tree list` (the snapshot's `syncing`/`sync_note`), or
    /// the `root_sync_changed` event a UI is already subscribed to.
    ///
    /// The ack is decoded through [`SyncData`] rather than discarded: it is one of
    /// the two acks on this surface (`accepted` here, `scheduled` for the whole-home
    /// nudge), and a daemon answering the wrong one is drift this decode catches.
    ///
    /// [`timeouts::NUDGE`]: crate::timeouts::NUDGE
    pub fn sync(&self, slug: &str) -> Result<SyncData, CliError> {
        self.post(
            "/api/roots/sync",
            &SyncRequest {
                slug: slug.to_owned(),
            },
            timeouts::NUDGE,
            "sync",
        )
    }

    /// `POST /api/doctor` — converge (or, under `dry_run`, merely diagnose) the
    /// worktree environment and return the report to render.
    ///
    /// Request/response, not a nudge: the daemon owns the mutation (single-realizer),
    /// and the CLI prints and exit-codes on what comes back.
    pub fn doctor(
        &self,
        slug: Option<&str>,
        dry_run: bool,
        fix: bool,
    ) -> Result<DoctorData, CliError> {
        self.post(
            "/api/doctor",
            &DoctorRequest {
                slug: slug.map(str::to_owned),
                dry_run,
                fix,
            },
            timeouts::OPS,
            "doctor",
        )
    }

    /// `POST /api/roots/remove {slug}` — ask a running daemon to delete a root and
    /// undeclare it, on the root's own lane (serialized against any in-flight
    /// reconcile/sync/fill, so the remove never races the realizer).
    pub fn remove_root(&self, slug: &str, force: bool) -> Result<(), CliError> {
        self.post::<_, serde_json::Value>(
            "/api/roots/remove",
            &RemoveRootRequest {
                slug: slug.to_owned(),
                force,
            },
            timeouts::OPS,
            "remove",
        )
        .map(drop)
    }

    /// `POST /api/worktrees/remove {slug, name}` — the worktree analogue of
    /// [`Self::remove_root`].
    pub fn remove_tree(&self, slug: &str, name: &str) -> Result<(), CliError> {
        self.post::<_, serde_json::Value>(
            "/api/worktrees/remove",
            &RemoveWorktreeRequest {
                slug: slug.to_owned(),
                name: name.to_owned(),
            },
            timeouts::OPS,
            "remove",
        )
        .map(drop)
    }

    /// `GET /api/roots` — the whole observable world: every declared root with its
    /// engine status, pool level and worktrees. What `grove tree list` renders when a
    /// daemon is up.
    pub fn snapshot(&self) -> Result<Snapshot, CliError> {
        let response = self
            .get("/api/roots", timeouts::SNAPSHOT)
            .map_err(|e| self.unreachable(&e))?;
        self.decode(response, "roots")
    }

    fn get(&self, path: &str, timeout: Duration) -> reqwest::Result<reqwest::blocking::Response> {
        self.http
            .get(format!("{}{path}", self.base))
            .timeout(timeout)
            .send()
    }

    /// POST a typed body and decode the envelope that comes back.
    fn post<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
        what: &str,
    ) -> Result<T, CliError> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .json(body)
            .timeout(timeout)
            .send()
            .map_err(|e| self.unreachable(&e))?;
        self.decode(response, what)
    }

    /// The one decode, and the one place the failure taxonomy is decided.
    ///
    /// Three outcomes, deliberately distinct:
    ///
    /// - **The body would not arrive** — the connection died mid-response. That is the
    ///   same class as a refused connect: `Daemon`, exit 4.
    /// - **The body is an error envelope** — the daemon answered, and said no. The
    ///   `ErrorCode` travels into the message so an operator can match what they see
    ///   against the contract table; exit 1 (see [`CliError`]'s `From<ApiError>` for
    ///   why the HTTP taxonomy is not remapped onto the exit codes).
    /// - **The body is not an envelope at all** — something is on the port that is not
    ///   a grove daemon, or a non-2xx with no body. `Api`, exit 1, naming the status.
    fn decode<T: DeserializeOwned>(
        &self,
        response: reqwest::blocking::Response,
        what: &str,
    ) -> Result<T, CliError> {
        let status = response.status();
        let body = response.bytes().map_err(|e| {
            CliError::Daemon(format!(
                "grove server at {} closed the connection mid-{what} ({e})",
                self.base
            ))
        })?;

        match serde_json::from_slice::<Envelope<T>>(&body) {
            Ok(Envelope::Ok(data)) if status.is_success() => Ok(data),
            // A success envelope under a failure status: the daemon never writes one,
            // so trust the status and report the failure rather than the payload.
            Ok(Envelope::Ok(_)) => Err(CliError::Api(format!(
                "grove server {what} failed (HTTP {status})"
            ))),
            Ok(Envelope::Err(e)) => Err(CliError::from(e)),
            Err(_) if !status.is_success() => Err(CliError::Api(format!(
                "grove server {what} failed (HTTP {status})"
            ))),
            Err(e) => Err(CliError::Api(format!("invalid {what} response: {e}"))),
        }
    }

    fn unreachable(&self, e: &reqwest::Error) -> CliError {
        CliError::Daemon(format!("cannot reach grove server at {} ({e})", self.base))
    }
}

/// The health status as an operator reads it. Total over the enum, so a second
/// health status added to the contract stops this compiling rather than printing
/// a debug spelling at a human.
const fn spelling(status: HealthStatus) -> &'static str {
    match status {
        HealthStatus::Ready => "ready",
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiClient, Reachability};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;
    use std::time::Duration;

    const BUDGET: Duration = Duration::from_millis(500);

    /// A throwaway HTTP server answering every request with `status` and `body`.
    fn server(status: &'static str, body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let _ = s.read(&mut [0u8; 1024]);
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        addr
    }

    /// An ephemeral port with nothing listening (bound, then dropped).
    fn closed_port() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    /// A listener that accepts and then never answers — the busy shape: the probe
    /// connects, then times out.
    fn hanging_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for s in listener.incoming().flatten() {
                thread::sleep(Duration::from_secs(30));
                drop(s);
            }
        });
        addr
    }

    #[test]
    fn health_ok_is_true_only_on_a_2xx() {
        let ready =
            r#"{"ok":true,"data":{"status":"ready","version":"0.1.0","home":"/home/.grove"}}"#;
        assert!(ApiClient::at(server("200 OK", ready), BUDGET).health_ok());

        let draining = r#"{"ok":false,"error":{"code":"unavailable","message":"server stopping"}}"#;
        let client = ApiClient::at(server("503 Service Unavailable", draining), BUDGET);
        assert!(!client.health_ok(), "a draining daemon is not ready");
        assert!(
            client.speaks_grove(),
            "…but it still identifies as grove, which gates the pid-less drain"
        );
        assert!(client.health().is_none(), "no payload without a ready 200");
    }

    #[test]
    fn health_decodes_the_envelope_through_the_contract_types() {
        let ready =
            r#"{"ok":true,"data":{"status":"ready","version":"9.9.9","home":"/home/.grove"}}"#;
        let client = ApiClient::at(server("200 OK", ready), BUDGET);
        assert_eq!(
            client.health().expect("a ready 200 carries it").version,
            "9.9.9"
        );
        assert_eq!(client.ready_version().as_deref(), Some("9.9.9"));
    }

    /// The self-update gate's two rejections: a draining 503 and a body that is not a
    /// success envelope. Both must read as "no version yet", never as an answer.
    #[test]
    fn ready_version_refuses_anything_but_a_ready_200() {
        let draining = r#"{"ok":false,"error":{"code":"unavailable","message":"draining"}}"#;
        assert!(
            ApiClient::at(server("503 Service Unavailable", draining), BUDGET)
                .ready_version()
                .is_none()
        );
        assert!(
            ApiClient::at(server("200 OK", "not json"), BUDGET)
                .ready_version()
                .is_none()
        );
        assert!(
            ApiClient::at(closed_port(), BUDGET)
                .ready_version()
                .is_none()
        );
    }

    /// A listener that is not grove: no envelope, so no shutdown POST is ever aimed
    /// at it.
    #[test]
    fn a_stranger_does_not_speak_grove() {
        assert!(!ApiClient::at(server("200 OK", "hello"), BUDGET).speaks_grove());
        assert!(!ApiClient::at(server("200 OK", r#"{"status":"ok"}"#), BUDGET).speaks_grove());
        assert!(!ApiClient::at(closed_port(), BUDGET).speaks_grove());
        assert!(!ApiClient::at(closed_port(), BUDGET).health_ok());
    }

    // ─── the tri-state, and the three listeners that produce it ──────────────

    #[test]
    fn reachable_is_offline_only_when_the_connection_is_refused() {
        let client = ApiClient::at(closed_port(), Duration::from_millis(300));
        assert_eq!(client.reachable(), Reachability::Offline);
    }

    #[test]
    fn reachable_is_up_when_a_server_answers() {
        let ready =
            r#"{"ok":true,"data":{"status":"ready","version":"0.1.0","home":"/home/.grove"}}"#;
        assert_eq!(
            ApiClient::at(server("200 OK", ready), BUDGET).reachable(),
            Reachability::Up
        );
        // Any HTTP answer, even one that says the daemon cannot work: it is still a
        // daemon, and it still owns realization.
        assert_eq!(
            ApiClient::at(server("500 Internal Server Error", ""), BUDGET).reachable(),
            Reachability::Up
        );
    }

    /// A daemon realizing somebody ELSE's home is not this home's realizer.
    ///
    /// The failure this closes: `GROVE_HOME=~/.grove-dev grove clone remove o/r` —
    /// the safe-looking way to exercise a second home — found the ordinary daemon on
    /// the default bind, delegated the remove to it, and deleted `~/.grove/code/o/r`
    /// with its uncommitted work, printing success. `GROVE_BIND` has its own default,
    /// so the mismatch is what happens *by default* whenever `GROVE_HOME` is
    /// overridden alone.
    #[test]
    fn a_daemon_serving_another_home_is_offline_for_this_one() {
        let ready =
            r#"{"ok":true,"data":{"status":"ready","version":"0.1.0","home":"/tmp/theirs"}}"#;
        let addr = server("200 OK", ready);

        assert_eq!(
            ApiClient::at(addr, BUDGET)
                .for_home("/tmp/mine")
                .reachable(),
            Reachability::Offline,
            "a stranger's daemon must not be handed this home's mutations"
        );
        assert_eq!(
            ApiClient::at(addr, BUDGET)
                .for_home("/tmp/theirs")
                .reachable(),
            Reachability::Up,
            "…and the daemon for OUR home still owns realization"
        );
        assert_eq!(
            ApiClient::at(addr, BUDGET).reachable(),
            Reachability::Up,
            "a client that named no home (process custody) is unaffected"
        );
    }

    /// Anything short of a decoded, ready payload naming a different home leaves the
    /// tri-state exactly as it was: "not proven different" is not "different".
    #[test]
    fn an_undecodable_health_payload_does_not_downgrade_reachability() {
        for body in [
            "not json",
            r#"{"ok":true,"data":{"status":"ready","version":"0.1.0"}}"#,
        ] {
            assert_eq!(
                ApiClient::at(server("200 OK", body), BUDGET)
                    .for_home("/tmp/mine")
                    .reachable(),
                Reachability::Up,
                "{body}"
            );
        }
        let draining = r#"{"ok":false,"error":{"code":"unavailable","message":"draining"}}"#;
        assert_eq!(
            ApiClient::at(server("503 Service Unavailable", draining), BUDGET)
                .for_home("/tmp/mine")
                .reachable(),
            Reachability::Up
        );
    }

    /// A hanging health endpoint must classify `Busy` — NOT `Offline` — so the caller
    /// delegates instead of racing a second in-process realizer into a dual clone.
    #[test]
    fn reachable_is_busy_when_the_server_hangs() {
        let client = ApiClient::at(hanging_server(), Duration::from_millis(300));
        assert_eq!(client.reachable(), Reachability::Busy);
    }

    // ─── the decode taxonomy ─────────────────────────────────────────────────

    #[test]
    fn remove_root_ok_envelope_succeeds() {
        let client = ApiClient::at(
            server("200 OK", r#"{"ok":true,"data":{"removed":"o/r"}}"#),
            BUDGET,
        );
        assert!(client.remove_root("o/r", false).is_ok());
    }

    /// Carried from v1 verbatim: an error envelope — at HTTP 200, which is how v1's
    /// Elixir remove route answered a not-found — is exit 1, and the server's error
    /// code reaches the operator's message.
    #[test]
    fn remove_tree_error_envelope_maps_to_api() {
        let client = ApiClient::at(
            server(
                "200 OK",
                r#"{"ok":false,"error":{"code":"not_found","message":"undeclared worktree"}}"#,
            ),
            BUDGET,
        );
        let err = client.remove_tree("o/r", "feat").unwrap_err();
        assert_eq!(err.exit_code(), 1, "an error envelope is Api (exit 1)");
        assert!(
            err.to_string().contains("not_found"),
            "surfaces the server's error code: {err}"
        );
    }

    /// The same code arriving with its documented status is still exit 1 — the HTTP
    /// taxonomy is not remapped onto the exit codes.
    #[test]
    fn a_404_error_envelope_is_still_api() {
        let client = ApiClient::at(
            server(
                "404 Not Found",
                r#"{"ok":false,"error":{"code":"not_found","message":"root not declared","data":{"slug":"o/r"}}}"#,
            ),
            BUDGET,
        );
        let err = client.remove_root("o/r", false).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("not_found"), "{err}");
    }

    #[test]
    fn remove_root_non_2xx_without_a_body_maps_to_api() {
        let client = ApiClient::at(server("503 Service Unavailable", ""), BUDGET);
        let err = client.remove_root("o/r", false).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("503"), "{err}");
    }

    /// Transport, not protocol: nothing answered, so exit 4 — the code `install.sh`
    /// and every script branch on for "the daemon is not there".
    #[test]
    fn a_connection_failure_maps_to_daemon() {
        let client = ApiClient::at(closed_port(), Duration::from_millis(300));
        assert_eq!(client.remove_root("o/r", false).unwrap_err().exit_code(), 4);
        assert_eq!(client.snapshot().unwrap_err().exit_code(), 4);
        assert_eq!(client.reconcile().unwrap_err().exit_code(), 4);
        assert_eq!(client.doctor(None, true, false).unwrap_err().exit_code(), 4);
        assert_eq!(client.sync("o/r").unwrap_err().exit_code(), 4);
        assert_eq!(client.ok().unwrap_err().exit_code(), 4);
    }

    /// The sync ack decodes through the contract type, and the *other* ack does not
    /// pass for it: `scheduled` is the whole-home nudge's answer, `accepted` is one
    /// root's, and a daemon confusing them would otherwise hand the CLI a promise it
    /// never made.
    #[test]
    fn sync_decodes_the_accepted_ack_and_refuses_the_nudge_s() {
        let accepted = ApiClient::at(
            server("200 OK", r#"{"ok":true,"data":{"sync":"accepted"}}"#),
            BUDGET,
        );
        assert_eq!(accepted.sync("o/r").unwrap(), grove_api::SyncData::ACCEPTED);

        let wrong = ApiClient::at(
            server("200 OK", r#"{"ok":true,"data":{"sync":"scheduled"}}"#),
            BUDGET,
        );
        let err = wrong.sync("o/r").unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("invalid sync response"), "{err}");
    }

    /// An undeclared slug comes back as the 404 error envelope the contract table
    /// names, and reaches the operator as exit 1 with the code and the slug in it.
    #[test]
    fn sync_surfaces_the_not_found_envelope() {
        let client = ApiClient::at(
            server(
                "404 Not Found",
                r#"{"ok":false,"error":{"code":"not_found","message":"root not declared","data":{"slug":"o/r"}}}"#,
            ),
            BUDGET,
        );
        let err = client.sync("o/r").unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("not_found"), "{err}");
    }

    /// A listener that answers, but not in this protocol: exit 1, naming the fact the
    /// body was not an envelope rather than pretending the daemon said something.
    #[test]
    fn a_body_that_is_not_an_envelope_is_api() {
        let client = ApiClient::at(server("200 OK", "<html>hello</html>"), BUDGET);
        let err = client.snapshot().unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("invalid roots response"), "{err}");
    }

    #[test]
    fn ok_reports_the_version_and_maps_an_unhealthy_daemon_to_exit_6() {
        let ready =
            r#"{"ok":true,"data":{"status":"ready","version":"0.1.0","home":"/home/.grove"}}"#;
        assert_eq!(
            ApiClient::at(server("200 OK", ready), BUDGET).ok().unwrap(),
            "grove server: ready (v0.1.0)"
        );

        let degraded = r#"{"ok":false,"error":{"code":"degraded","message":"server degraded"}}"#;
        let err = ApiClient::at(server("503 Service Unavailable", degraded), BUDGET)
            .ok()
            .unwrap_err();
        assert_eq!(err.exit_code(), 6, "reachable but not ready is Unhealthy");
        assert!(err.to_string().contains("degraded"), "{err}");
    }

    #[test]
    fn reconcile_maps_a_refusing_server_to_api() {
        let client = ApiClient::at(server("503 Service Unavailable", ""), BUDGET);
        let err = client.reconcile().unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("refused to reconcile"), "{err}");
    }

    /// The doctor payload decodes through the contract types, `error.data` and all —
    /// and an empty `checks`/`pools`/`statuses` is a valid answer, not a decode error.
    #[test]
    fn doctor_decodes_the_contract_payload() {
        let body = r#"{"ok":true,"data":{"report":[
            {"slug":"o/r","path":".env","status":"conflict","reason":"real file"}
        ],"statuses":[{"slug":"o/r","status":"degraded"}]}}"#;
        let data = ApiClient::at(server("200 OK", body), BUDGET)
            .doctor(Some("o/r"), false, false)
            .unwrap();
        assert_eq!(data.report.len(), 1);
        assert_eq!(data.statuses[0].status, grove_api::RootStatus::Degraded);
        assert!(data.pools.is_empty() && data.checks.is_empty());
    }

    #[test]
    fn a_snapshot_decodes_through_the_contract_types() {
        let body = r#"{"ok":true,"data":{"roots":[{
            "slug":"o/r","url":"file:///src","status":"ready",
            "pool":{"observed":1,"target":2},"syncing":false,
            "trunk":"/home/code/o/r/.trunk","worktrees":[
              {"name":"feat","branch":"feature/x","declared":true,"present":true,
               "path":"/home/code/o/r/feat"}
            ]}],"logs":[]}}"#;
        let snapshot = ApiClient::at(server("200 OK", body), BUDGET)
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.roots[0].slug, "o/r");
        assert_eq!(snapshot.roots[0].pool.target, 2);
        assert_eq!(snapshot.roots[0].worktrees[0].name, "feat");
    }
}
