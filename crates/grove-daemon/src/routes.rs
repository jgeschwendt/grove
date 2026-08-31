//! The eight handlers. Every body is a `grove-api` type carried by an
//! [`Envelope`](grove_api::Envelope); nothing here writes JSON by hand.
//!
//! The four ops routes reproduce v1's controller semantics exactly, including the
//! validation the Elixir side did *before* calling grove-ops: the ops are idempotent
//! and would happily acknowledge a no-op, so an undeclared slug or worktree is a 404
//! here rather than a cheerful `{"removed": …}`.
//!
//! **Every git write here runs on its root's lane** (carried law 6, and law 6's
//! "doctor included"): a remove racing the engine's reconcile would delete a tree
//! mid-clone, and a doctor converge racing it would repoint links under a moving
//! worktree. What stays off the lane is the manifest read the remove routes do first
//! — it writes nothing, and putting it behind an in-flight clone would make an
//! existence check wait an hour to say "no such root".

use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use grove_api::routes::{
    DoctorData, DoctorRequest, HealthData, HealthStatus, ReconcileData, RemoveRootData,
    RemoveRootRequest, RemoveWorktreeData, RemoveWorktreeRequest, ShutdownData, SyncData,
    SyncRequest, VersionData,
};
use grove_api::{ApiError, BootStatus, ErrorCode};
use grove_ops::doctor::{Check, CheckKind, CheckStatus};
use serde_json::json;

use crate::app::{AppState, SHUTDOWN_GRACE};
use crate::lane::{LaneError, Priority};
use crate::reply::{Reply, error_response};

/// How long one root has to complete its doctor pass. v1's `:doctor_lane_timeout`.
///
/// Generous, because the pass queues behind whatever that root's lane is already
/// doing and a clone can run for an hour — the budget exists so one wedged root
/// cannot hold a whole-home report open forever, not to cap honest work.
pub const DOCTOR_BUDGET: Duration = Duration::from_secs(30);

/// `GET /api/health` — the only 200 this route can produce says `ready`; every
/// other boot state is a 503 error envelope carrying the state itself.
pub async fn health(State(state): State<AppState>) -> Reply<HealthData> {
    match state.boot.status() {
        BootStatus::Ready => Reply::ok(HealthData {
            status: HealthStatus::Ready,
            version: state.config.version.clone(),
            // Which home this daemon realizes. A client resolves `GROVE_HOME` and
            // `GROVE_BIND` independently, so without this a `roots/remove` aimed at
            // one home is executed against another's and nothing on the wire could
            // tell — see `HealthData`.
            home: state.config.home.display().to_string(),
        }),
        // Up but unable to do its job. The reason is the payload the self-update
        // health gate rolls a bad bundle back on, so it travels rather than being
        // flattened into the message.
        status @ BootStatus::Degraded { .. } => Reply::fail(
            StatusCode::SERVICE_UNAVAILABLE,
            ApiError::new(ErrorCode::Degraded, "server degraded").with_data(data_of(&status)),
        ),
        status => Reply::fail(
            StatusCode::SERVICE_UNAVAILABLE,
            ApiError::new(
                ErrorCode::Unavailable,
                format!("server {}", status.as_str()),
            )
            .with_data(data_of(&status)),
        ),
    }
}

/// `GET /api/daemon/version` — version plus uptime, the latter read through the
/// clock seam.
pub async fn version(State(state): State<AppState>) -> Reply<VersionData> {
    Reply::ok(VersionData {
        version: state.config.version.clone(),
        uptime_ms: state.boot.uptime_ms(&*state.clock),
    })
}

/// `POST /api/daemon/shutdown` — mark stopping, acknowledge, then drain.
///
/// The state flips *before* the response is written, so nothing slips past the
/// readiness gate behind an acknowledgement that the daemon is going away. The
/// drain itself is scheduled after a short grace for the same reason a library
/// never calls `exit`: the caller gets its answer, and the process ends when the
/// accept loop stops, not when a handler decides to.
pub async fn shutdown(State(state): State<AppState>) -> Reply<ShutdownData> {
    state.boot.mark_stopping();
    if state.config.enable_shutdown {
        state.shutdown.schedule(SHUTDOWN_GRACE);
    }
    Reply::ok(ShutdownData::STOPPING)
}

/// `POST /api/roots/reconcile` — the reliable nudge. The CLI calls it after
/// declaring a manifest change while a server is up (single-realizer), so
/// convergence never depends on the fs watcher noticing the write.
pub async fn reconcile(State(state): State<AppState>) -> Reply<ReconcileData> {
    state.reconcile.poke();
    Reply::ok(ReconcileData::SCHEDULED)
}

/// `POST /api/roots/sync {slug}` — fetch this root's default branch and fast-forward
/// its `.trunk`.
///
/// **Accept-only**, exactly as [`Engine::sync`] is: the handler returns as soon as the
/// root's engine has recorded the request, never when the fetch lands. That is not a
/// convenience — it is carried law 8 at the HTTP edge. A route that waited would hold
/// a connection open for the length of a network fetch behind whatever else that
/// root's lane is doing, and would have to invent a second, request-scoped answer for
/// a fact the daemon already publishes twice: `root_sync_changed` on
/// `GET /api/events` (broadcast on accept *and* on completion), and the
/// `syncing`/`sync_note` fields every snapshot carries. So the ack is the contract's
/// whole content, and completion is observed, never returned.
///
/// Three outcomes, and the order they are decided in matters:
///
/// 1. **Not declared** → 404. Like the removes, the check reads the manifest — off
///    any lane, because it writes nothing and an existence question must not wait out
///    an hour-long clone. Without it an undeclared slug would fall into the
///    no-engine case below and be told to retry forever.
/// 2. **Declared, no engine driving it** → 503 `unavailable`. Nothing would record
///    the request, so accepting it would be a lie: the engine set has not started
///    this root yet (a window measured in milliseconds after a declare), or this
///    daemon has no engine room at all. "Come back", in the readiness vocabulary.
/// 3. **Declared, engine running** → 200 `{"sync": "accepted"}`.
///
/// No lane is taken here. The engine owns the dispatch onto the root's lane, under
/// its single background slot, so a burst of sync requests coalesces into one fetch
/// (invariant `push-only`) instead of queueing one lane job each.
///
/// # `accepted` is recorded, not scheduled
///
/// The engine dispatches a pending sync only once the root is `ready` — syncing an
/// unrealized root is meaningless — so `accepted` means *the request is latched*, not
/// *a fetch is coming*. From `cloning` or `missing` that latch is the whole point: the
/// sync runs on the drive that follows the reconcile, which is why the route does not
/// refuse there.
///
/// `degraded` is the corner with no clock. The engine leaves it only on a dispatched
/// reconcile — a manifest change or `POST /api/roots/reconcile` — so until one arrives
/// the root reports `syncing: true` with no second `root_sync_changed` behind it. A
/// client must read `syncing` as "this root holds a sync request", never as "a fetch is
/// in flight"; `status` is what says whether anything can run.
///
/// [`Engine::sync`]: crate::engine::Engine::sync
pub async fn sync(
    State(state): State<AppState>,
    body: Result<Json<SyncRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return invalid_request("missing slug");
    };
    let home = state.config.home.clone();
    let slug = request.slug;

    let declared = match ops({
        let (home, slug) = (home, slug.clone());
        move || grove_ops::roots::list(&home).map(|roots| roots.iter().any(|r| r.slug == slug))
    })
    .await
    {
        Err(response) => return response,
        // The manifest would not read at all. Not a 404 — "I cannot tell" is not
        // "no such root" — so it answers the same 422 a failed op does, with the
        // reason the operator needs to fix it.
        Ok(Err(e)) => return sync_failed(&e.to_string()),
        Ok(Ok(declared)) => declared,
    };
    if !declared {
        return error_response(
            StatusCode::NOT_FOUND,
            ApiError::new(ErrorCode::NotFound, "root not declared")
                .with_data(json!({"slug": slug})),
        );
    }

    // `None` covers both shapes of "nothing is driving this root": no engine room at
    // all, and an engine room that has not started this slug's driver yet.
    let engine = match state.engines.get() {
        Some(engines) => engines.engine(&slug).await,
        None => None,
    };
    let Some(engine) = engine else {
        return no_engine(&slug);
    };
    match engine.sync().await {
        Ok(()) => Reply::ok(SyncData::ACCEPTED).into_response(),
        // The driver disappeared between the lookup and the send — an undeclare
        // landing mid-request. That is the no-engine case one instant later, and it
        // answers as such rather than inventing a fourth outcome.
        Err(_) => no_engine(&slug),
    }
}

/// `POST /api/roots/remove` — undeclare a root and delete it from disk.
pub async fn remove_root(
    State(state): State<AppState>,
    body: Result<Json<RemoveRootRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return invalid_request("missing slug");
    };
    let home = state.config.home.clone();
    let slug = request.slug;
    let removal = if request.force {
        grove_ops::roots::Removal::Forced
    } else {
        grove_ops::roots::Removal::Guarded
    };

    let declared = match ops({
        let (home, slug) = (home.clone(), slug.clone());
        move || grove_ops::roots::list(&home).map(|roots| roots.iter().any(|r| r.slug == slug))
    })
    .await
    {
        Err(response) => return response,
        Ok(Err(e)) => return remove_failed(&e),
        Ok(Ok(declared)) => declared,
    };
    if !declared {
        return error_response(
            StatusCode::NOT_FOUND,
            ApiError::new(ErrorCode::NotFound, "root not declared")
                .with_data(json!({"slug": slug})),
        );
    }

    // On the lane: delete-on-disk-then-undeclare must not interleave with this
    // root's in-flight reconcile, which would otherwise observe the half-deleted
    // root as declared-but-missing and re-clone what the operator just removed.
    match lane(&state, &slug, {
        let (home, slug) = (home, slug.clone());
        move || grove_ops::roots::remove(&home, &slug, removal)
    })
    .await
    {
        Err(response) => response,
        Ok(Err(e)) => remove_failed(&e),
        Ok(Ok(())) => {
            // The nudge v1 sent the watcher: a removed root's engine is torn down by
            // the reconcile pass that follows, never by an explicit stop call.
            state.reconcile.poke();
            Reply::ok(RemoveRootData { removed: slug }).into_response()
        }
    }
}

/// `POST /api/worktrees/remove` — git-remove one worktree, then undeclare it. No
/// reconcile nudge: unlike a root, a worktree has no engine to tear down.
pub async fn remove_worktree(
    State(state): State<AppState>,
    body: Result<Json<RemoveWorktreeRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return invalid_request("missing slug or name");
    };
    let home = state.config.home.clone();
    let (slug, name) = (request.slug, request.name);

    // On the lane, unlike the root check above: listing worktrees runs `git worktree
    // list` and one `git status` per tree against the same repository the engine may
    // be mid-reconcile in.
    let declared = match lane(&state, &slug, {
        let (home, slug, name) = (home.clone(), slug.clone(), name.clone());
        move || {
            grove_ops::worktrees::list(&home, &slug)
                .map(|trees| trees.iter().any(|w| w.name == name && w.declared))
        }
    })
    .await
    {
        Err(response) => return response,
        Ok(Err(e)) => return remove_failed(&e),
        Ok(Ok(declared)) => declared,
    };
    if !declared {
        return error_response(
            StatusCode::NOT_FOUND,
            ApiError::new(ErrorCode::NotFound, "worktree not declared")
                .with_data(json!({"slug": slug, "name": name})),
        );
    }

    match lane(&state, &slug, {
        let (home, slug, name) = (home, slug.clone(), name.clone());
        move || grove_ops::worktrees::remove(&home, &slug, &name)
    })
    .await
    {
        Err(response) => response,
        Ok(Err(e)) => remove_failed(&e),
        Ok(Ok(())) => Reply::ok(RemoveWorktreeData { removed: name }).into_response(),
    }
}

/// `POST /api/doctor` — diagnose (`dry_run`) or converge the worktree environment,
/// report pool levels beside the share report, and run the git-plumbing checks.
///
/// A body-less POST is a whole-home, non-dry, non-fixing run — v1's controller read
/// its params with a strict `== true`, so an absent field was false. A body that is
/// present but malformed is still a 422: that is a client bug, not a default.
///
/// The raw body, not an optional `Json`: axum's optional extractor reads a request
/// with no `Content-Type` at all as *no body*, so `{"slug":"o/r","dry_run":true}` sent
/// unlabelled would silently become the default — a scoped dry run executed as a
/// whole-home materializing converge. Emptiness decides whether there is a body;
/// the content type is then a requirement, not a discriminator.
///
/// **Per-root, on each root's own lane, even for the whole-home form**, and under a
/// [`DOCTOR_BUDGET`] per root. Doctor mutates symlinks inside worktrees and reads git
/// beside them; running the sweep off-lane — or on one root's lane — would race the
/// engine reconciling any of the others. Roots run concurrently, and their reports
/// concatenate in declared order, exactly as the single composed call would have
/// produced them.
///
/// **A slow root is skipped, not fatal — in the whole-home form.** v1's fan-out let a
/// root that missed its budget contribute nothing while every other root's answer
/// still came back; here it contributes a `root`/`unavailable` check, so it is named
/// rather than merely absent, and its engine status still travels in `statuses`. A
/// request that *named* a slug is answered strictly: the caller asked about one root
/// and an empty report would read as "nothing wrong".
///
/// `statuses` carries every running engine's status, and is the only place `cloning`
/// and `degraded` are observable: neither is derivable from disk, so a root mid-clone
/// or stopped on a terminal fault is invisible to every other check doctor runs. A
/// daemon with no engine room (a caller that claimed the reconcile mailbox) reports
/// none, which is honest — nothing is driving those roots.
pub async fn doctor(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let request = match doctor_request(&headers, &body) {
        Ok(request) => request,
        Err(message) => return invalid_request(message),
    };
    let home = state.config.home.clone();
    let scoped = request.slug.is_some();

    let slugs = match request.slug.clone() {
        Some(slug) => vec![slug],
        None => match ops({
            let home = home.clone();
            move || grove_ops::roots::list(&home)
        })
        .await
        {
            Err(response) => return response,
            Ok(Err(e)) => return doctor_failed(&e.to_string()),
            Ok(Ok(roots)) => roots.into_iter().map(|root| root.slug).collect(),
        },
    };

    let mut data = DoctorData::default();
    // The manifest is one file, so its check is one row for the whole run — off any
    // lane, because it reads no git and must answer even when every root is wedged.
    match ops({
        let home = home.clone();
        move || grove_ops::doctor::manifest_checks(&home)
    })
    .await
    {
        Err(response) => return response,
        Ok(checks) => data.checks = checks,
    }

    // Spawned first, awaited after: the roots run on their own lanes concurrently,
    // and awaiting the handles in order is what keeps the report in declared order.
    let passes: Vec<_> = slugs
        .iter()
        .map(|slug| {
            let (state, slug) = (state.clone(), slug.clone());
            tokio::spawn(async move { root_pass(&state, slug, request.dry_run, request.fix).await })
        })
        .collect();

    for (slug, pass) in slugs.into_iter().zip(passes) {
        let outcome = pass
            .await
            .unwrap_or_else(|e| Err(format!("the doctor pass panicked: {e}")));
        match outcome {
            Ok(pass) => {
                data.report.extend(pass.report);
                data.pools.extend(pass.pools);
                data.checks.extend(pass.checks);
            }
            // The caller named this root; an empty report would read as "nothing
            // wrong" about the one thing it asked about.
            Err(reason) if scoped => return doctor_failed(&reason),
            Err(reason) => data.checks.push(unavailable_root(&slug, reason)),
        }
    }

    if let Some(engines) = state.engines.get() {
        data.statuses = engines.statuses(request.slug.as_deref()).await;
    }
    Reply::ok(data).into_response()
}

/// One root's doctor pass: the share converge, the pool read, and the plumbing
/// checks, as a single job on that root's lane.
///
/// One job because they are one round trip: splitting them would put the checks
/// behind whatever the engine queued between the two, and read a repository the
/// converge had already moved on from.
async fn root_pass(
    state: &AppState,
    slug: String,
    dry_run: bool,
    fix: bool,
) -> Result<RootPass, String> {
    let home = state.config.home.clone();
    let job = state.lanes.run(&slug, Priority::Foreground, {
        let slug = slug.clone();
        move || {
            let (report, pools) = grove_ops::doctor::run(&home, Some(&slug), dry_run, fix)?;
            Ok::<_, grove_ops::Error>(RootPass {
                report,
                pools,
                checks: grove_ops::doctor::root_checks(&home, &slug),
            })
        }
    });

    let deadline = state.clock.deadline(DOCTOR_BUDGET);
    match crate::wait::within(deadline, &*state.clock, job).await {
        Some(Ok(Ok(pass))) => Ok(pass),
        Some(Ok(Err(e))) => Err(e.to_string()),
        Some(Err(e)) => Err(e.to_string()),
        None => Err(format!(
            "the root exceeded doctor's {}s budget",
            DOCTOR_BUDGET.as_secs()
        )),
    }
}

/// What one root contributes to a doctor report.
struct RootPass {
    report: Vec<grove_ops::env::ShareOutcome>,
    pools: Vec<grove_ops::pool::PoolStatus>,
    checks: Vec<Check>,
}

/// The row a root that did not answer contributes to a whole-home report.
fn unavailable_root(slug: &str, reason: String) -> Check {
    Check {
        check: CheckKind::Root,
        status: CheckStatus::Unavailable,
        slug: Some(slug.to_owned()),
        name: None,
        detail: Some(reason),
    }
}

/// The doctor body's decode: empty is the documented default run, anything else must
/// be JSON and must say so. An unlabelled non-empty body is refused rather than
/// ignored — silently dropping it turns the caller's request into a different, more
/// destructive one.
fn doctor_request(headers: &HeaderMap, body: &[u8]) -> Result<DoctorRequest, &'static str> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(DoctorRequest::default());
    }
    if !is_json(headers) {
        return Err("doctor request body must be sent as application/json");
    }
    serde_json::from_slice(body).map_err(|_| "malformed doctor request")
}

/// Whether the request declares a JSON body. Follows axum's own `Json` rule: the
/// `application/json` essence or any `application/…+json` structured suffix, with
/// parameters (`; charset=utf-8`) ignored.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

/// No such route. Rendered in the envelope like everything else — v1's framework
/// fallback wrote a *second* hand-built copy of the shape, and this crate has one.
pub async fn not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        ApiError::new(ErrorCode::Error, "Not Found"),
    )
}

/// The route exists but not for this method.
pub async fn method_not_allowed() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        ApiError::new(ErrorCode::Error, "Method Not Allowed"),
    )
}

/// Run a synchronous grove-ops call off the async worker threads, **without** taking
/// a lane — for reads that write nothing and must not queue behind a clone. The
/// `Err` half is already a response: a panicking op is not part of any route's
/// vocabulary, so it answers 500 `error` rather than being dressed up as a domain
/// failure.
async fn ops<T: Send + 'static>(call: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(call).await.map_err(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::new(ErrorCode::Error, format!("ops task failed: {e}")),
        )
    })
}

/// Run a grove-ops call on `slug`'s lane, serialized against every other writer for
/// that root.
///
/// Foreground: a client is waiting on this request, so it drains ahead of the
/// engine's queued convergence work — but never preempts the op already running, so
/// a remove issued mid-clone waits out that clone.
async fn lane<T: Send + 'static>(
    state: &AppState,
    slug: &str,
    call: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Response> {
    state
        .lanes
        .run(slug, Priority::Foreground, call)
        .await
        .map_err(|e| match e {
            // v1's `ops_busy`: the root's queue is at its bound, which means it is
            // wedged rather than merely busy. 503 with the readiness vocabulary —
            // "come back", not "your request was wrong".
            LaneError::Busy => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                ApiError::new(ErrorCode::Unavailable, e.to_string())
                    .with_data(json!({"slug": slug})),
            ),
            LaneError::Lost => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::new(ErrorCode::Error, format!("ops task failed: {e}")),
            ),
        })
}

fn invalid_request(message: &str) -> Response {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        ApiError::new(ErrorCode::InvalidRequest, message),
    )
}

/// No engine is driving the named root, so nothing could record a sync request.
///
/// 503 rather than 404 or 500: the root **is** declared, and the engine set starts one
/// on the next `roots_changed` — so this is a "come back", the same shape a saturated
/// lane answers with. Accepting instead would drop the request on the floor while
/// telling the caller it was recorded.
fn no_engine(slug: &str) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ApiError::new(
            ErrorCode::Unavailable,
            "no engine is driving this root yet; retry in a moment",
        )
        .with_data(json!({"slug": slug})),
    )
}

/// A sync request that could not be decided because the manifest would not read.
fn sync_failed(reason: &str) -> Response {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        ApiError::new(ErrorCode::SyncFailed, "sync failed").with_data(json!({"reason": reason})),
    )
}

/// A doctor run that reached the ops layer and failed there.
fn doctor_failed(reason: &str) -> Response {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        ApiError::new(ErrorCode::DoctorFailed, "doctor failed")
            .with_data(json!({"reason": reason})),
    )
}

/// The failure both remove routes share. `error.data.reason` carries the ops
/// message; the category (`grove_ops::Error::code`) stays out of the HTTP code,
/// which classifies the *request*, not the operation.
fn remove_failed(e: &grove_ops::Error) -> Response {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        ApiError::new(ErrorCode::RemoveFailed, "remove failed")
            .with_data(json!({"reason": e.to_string()})),
    )
}

/// A boot status as `error.data` — `{"status":"stopping"}`, or the degraded variant
/// with its reason beside it.
fn data_of(status: &BootStatus) -> serde_json::Value {
    serde_json::to_value(status).unwrap_or_else(|_| json!({"status": status.as_str()}))
}

#[cfg(test)]
mod tests {
    use super::data_of;
    use grove_api::BootStatus;
    use serde_json::json;

    /// The `error.data` payloads the contract table names, byte for byte.
    #[test]
    fn health_error_data_is_the_boot_status_object() {
        assert_eq!(
            data_of(&BootStatus::Stopping),
            json!({"status": "stopping"})
        );
        assert_eq!(data_of(&BootStatus::Booting), json!({"status": "booting"}));
        assert_eq!(
            data_of(&BootStatus::Degraded {
                reason: "ops_incompatible".into()
            }),
            json!({"status": "degraded", "reason": "ops_incompatible"}),
            "the reason a health gate rolls back on travels in the payload"
        );
    }
}
