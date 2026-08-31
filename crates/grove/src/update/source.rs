//! Release resolution + HTTP fetch: where update bundles come from (GitHub
//! Releases in production; flat `LocalDir`/`BaseUrl` for the smoke tests).
//!
//! v2 publishes to **this repo's own Releases** rather than v1's separate download
//! satellite. The repo is public, so both halves work anonymously: channel resolution
//! reads the releases list, and the bundle comes off the plain `releases/download/…`
//! URL. A `GH_TOKEN`/`GITHUB_TOKEN` in the environment is attached to the *resolution*
//! calls only (sent to github.com and nowhere else) purely to lift GitHub's anonymous
//! rate limit — nothing here requires one.
//!
//! [`scripts/install.sh`] stays the bootstrap for a box with no `grove` yet: it stages
//! the two files into a directory and hands `grove up` the
//! [`BundleSource::LocalDir`] it already understands — the same bash/Rust split v1
//! drew.
//!
//! [`scripts/install.sh`]: https://github.com/jgeschwendt/grove/blob/main/scripts/install.sh

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::layout::version_key;
use crate::CliError;

/// Page size and page cap for walking the GitHub releases list when resolving a
/// prerelease channel (a canary can sit past the first page).
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 20;

/// GitHub repo the release channels resolve against — where `install.sh` and
/// `release.yml` publish. Overridable for tests via `GROVE_INSTALL_BASE_URL` (an http
/// base, or a local fixture dir the install smoke test points at).
const RELEASE_REPO: &str = "jgeschwendt/grove";

/// Channel followed when none is pinned/persisted. Stable = the `releases/latest`
/// redirect; other channels are the highest `v*-<channel>.N` prerelease.
pub(super) const DEFAULT_CHANNEL: &str = "stable";

/// Classify the `GROVE_INSTALL_BASE_URL` override into a [`BundleSource`]. An
/// `https://` base is allowed anywhere; a plain `http://` base is refused unless
/// it targets loopback (`127.0.0.1`/`localhost`) — the test/CI seam — so a MITM
/// can't redirect a self-update over cleartext. A non-URL value is a local
/// fixture dir (the smoke tests); `None` → GitHub Releases.
pub(super) fn install_source(base: Option<String>) -> Result<BundleSource, CliError> {
    let Some(base) = base else {
        return Ok(BundleSource::GitHubReleases {
            repo: RELEASE_REPO.to_string(),
        });
    };
    if let Some(rest) = base.strip_prefix("http://") {
        // authority = up to the first '/'. RFC 3986 allows `userinfo@real-host`
        // there — `http://127.0.0.1@evil.com/x` targets evil.com while a naive
        // host parse reads 127.0.0.1 — so refuse ANY userinfo outright (the
        // loopback test seam never needs credentials). Then peel an optional
        // [..] IPv6 bracket (so `[::1]:8080` → `::1`), else strip a trailing :port.
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.strip_prefix('[').map_or_else(
            || authority.split(':').next().unwrap_or(""),
            |b| b.split(']').next().unwrap_or(""),
        );
        if authority.contains('@') || !is_loopback_host(host) {
            return Err(CliError::Update(format!(
                "refusing plain-http GROVE_INSTALL_BASE_URL {base:?}: use https:// \
                 (a 127.0.0.1/localhost base is allowed for testing)"
            )));
        }
        Ok(BundleSource::BaseUrl(base))
    } else if base.starts_with("https://") {
        Ok(BundleSource::BaseUrl(base))
    } else {
        Ok(BundleSource::LocalDir(PathBuf::from(base)))
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Where update bundles come from. `GitHubReleases` in production; the flat
/// `LocalDir`/`BaseUrl` schemes back the smoke tests and `install.sh`'s staging.
///
/// - `GitHubReleases`: a channel resolves to a `v<version>` tag on the repo's
///   Releases; assets download from `.../releases/download/v<version>/<target>.tar.gz`.
/// - `LocalDir`/`BaseUrl`: `<base>/<version>/<target>.tar.gz` (+ `.sha256`) with a
///   `<base>/latest` text file naming the current version (channel-agnostic).
#[derive(Debug)]
pub enum BundleSource {
    GitHubReleases { repo: String },
    LocalDir(PathBuf),
    BaseUrl(String),
}

impl BundleSource {
    /// Resolve the version a `channel` currently points at. For the flat
    /// `LocalDir`/`BaseUrl` schemes the channel is ignored (they carry a single
    /// `latest` file — the smoke tests). For `GitHubReleases`, `stable` follows the
    /// `releases/latest` redirect and any other channel is the highest matching
    /// `v*-<channel>.N` prerelease (mirrors `scripts/install.sh`).
    pub fn channel_version(&self, channel: &str) -> Result<String, CliError> {
        match self {
            BundleSource::LocalDir(dir) => Ok(fs::read_to_string(dir.join("latest"))
                .map_err(|e| CliError::Update(format!("read latest: {e}")))?
                .trim()
                .to_string()),
            BundleSource::BaseUrl(base) => Ok(http_get_text(&format!("{base}/latest"), None)?
                .trim()
                .to_string()),
            BundleSource::GitHubReleases { repo } if channel == "stable" => {
                resolve_stable_version(repo)
            }
            BundleSource::GitHubReleases { repo } => resolve_channel_paged(channel, |page| {
                http_get_text(
                    &format!(
                        "https://api.github.com/repos/{repo}/releases?per_page={RELEASES_PER_PAGE}&page={page}"
                    ),
                    github_token().as_deref(),
                )
            }),
        }
    }

    /// Fetch the bundle for `version`/`target`, returning the `.tar.gz` bytes and
    /// the expected sha256 (hex) from the sidecar `.sha256` file.
    pub(super) fn fetch(&self, version: &str, target: &str) -> Result<(Vec<u8>, String), CliError> {
        let rel = format!("{version}/{target}.tar.gz");
        let (bytes, sha) = match self {
            BundleSource::LocalDir(dir) => {
                let tar = fs::read(dir.join(&rel))
                    .map_err(|e| CliError::Update(format!("read {rel}: {e}")))?;
                let sha = fs::read_to_string(dir.join(format!("{rel}.sha256")))
                    .map_err(|e| CliError::Update(format!("read {rel}.sha256: {e}")))?;
                (tar, sha)
            }
            BundleSource::BaseUrl(base) => {
                let tar = http_get_bytes(&format!("{base}/{rel}"), None)?;
                let sha = http_get_text(&format!("{base}/{rel}.sha256"), None)?;
                (tar, sha)
            }
            BundleSource::GitHubReleases { repo } => {
                // The public asset URL. A token is deliberately NOT sent here: the
                // download 302s to a storage host, and a credential must not follow a
                // redirect off github.com. Public assets need none anyway.
                let base = format!("https://github.com/{repo}/releases/download/v{version}");
                let tar = http_get_bytes(&format!("{base}/{target}.tar.gz"), None)?;
                let sha = http_get_text(&format!("{base}/{target}.tar.gz.sha256"), None)?;
                (tar, sha)
            }
        };
        // The sidecar may be `<hex>` or `<hex>  filename`; take the first field.
        let sha = sha.split_whitespace().next().unwrap_or("").to_lowercase();
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CliError::Update(format!(
                "{rel}.sha256 is not a sha256 hex digest"
            )));
        }
        Ok((bytes, sha))
    }
}

/// Follow `github.com/<repo>/releases/latest` (a redirect to the tag page) and
/// read the version off the final URL. reqwest follows redirects by default
/// (`Policy::limited(10)`), so the resolved tag is `resp.url()`'s `/tag/<tag>`.
/// `releases/latest` excludes prereleases — correct for the stable channel.
fn resolve_stable_version(repo: &str) -> Result<String, CliError> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let resp = http_client(github_token().as_deref())?
        .get(&url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| CliError::Update(format!("GET {url}: {e}")))?;
    let final_url = resp.url().as_str();
    tag_from_release_url(final_url).ok_or_else(|| {
        CliError::Update(format!(
            "could not resolve latest stable release ({final_url})"
        ))
    })
}

/// Version from a `.../releases/tag/<tag>` URL (the `releases/latest` redirect
/// target), leading `v` stripped. `None` if the URL isn't a tag page.
fn tag_from_release_url(url: &str) -> Option<String> {
    let (_, tag) = url.rsplit_once("/tag/")?;
    let tag = tag.split(['?', '#']).next()?.trim_end_matches('/');
    (!tag.is_empty()).then(|| tag.trim_start_matches('v').to_string())
}

/// GitHub's releases list is paginated (30/page by default); a canary may sit
/// past the first page, so walk pages until one yields a match or we reach the
/// end. Releases come newest-first, so the highest of a channel lives on the
/// first page that carries the channel at all — take it and stop. A short page
/// (< `RELEASES_PER_PAGE`) is the last one. `fetch_page(n)` is injected for tests.
fn resolve_channel_paged(
    channel: &str,
    fetch_page: impl Fn(usize) -> Result<String, CliError>,
) -> Result<String, CliError> {
    for page in 1..=MAX_RELEASE_PAGES {
        let (best, count) = page_best(&fetch_page(page)?, channel)?;
        if let Some(v) = best {
            return Ok(v);
        }
        if count < RELEASES_PER_PAGE {
            break; // last page, no match
        }
    }
    Err(CliError::Update(format!(
        "no {channel} releases found on {RELEASE_REPO}"
    )))
}

/// Highest `v<base>-<channel>.<N>` version in one GitHub releases-list JSON body,
/// leading `v` stripped, plus how many releases the page held (to detect the last
/// page). The channel (env/file-controlled) is matched by a literal suffix parse —
/// never compiled into a regex. Numeric-aware sort via [`version_key`], so
/// `v0.1.1-canary.1` outranks `v0.1.0-canary.30`.
fn page_best(releases_json: &str, channel: &str) -> Result<(Option<String>, usize), CliError> {
    let releases: Vec<serde_json::Value> = serde_json::from_str(releases_json)
        .map_err(|e| CliError::Update(format!("parse releases list: {e}")))?;
    let suffix = format!("-{channel}.");
    let best = releases
        .iter()
        .filter_map(|r| r.get("tag_name")?.as_str())
        .filter(|tag| channel_tag_matches(tag, &suffix))
        .max_by(|a, b| version_key(a).cmp(&version_key(b)))
        .map(|tag| tag.trim_start_matches('v').to_string());
    Ok((best, releases.len()))
}

/// Single-page channel resolution — the pure core exercised by unit tests.
#[cfg(test)]
fn select_channel_version(releases_json: &str, channel: &str) -> Result<String, CliError> {
    page_best(releases_json, channel)?
        .0
        .ok_or_else(|| CliError::Update(format!("no {channel} releases found on {RELEASE_REPO}")))
}

/// A tag belongs to the channel iff `<suffix>` (`-<channel>.`) is present and
/// followed by a non-empty run of digits to the end (`v0.1.1-canary.2` ✓;
/// `v0.1.1-canary` ✗; `v0.1.1-beta.2` ✗ for `-canary.`).
fn channel_tag_matches(tag: &str, suffix: &str) -> bool {
    match tag.rfind(suffix) {
        Some(i) => {
            let n = &tag[i + suffix.len()..];
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Platforms this box could ask for and the release line does not publish, with the
/// reason an operator needs. Kept in step with two other lists by construction: the
/// matrix in `.github/workflows/release.yml` (which builds the bundles) and the
/// refusal in `scripts/install.sh` (which says the same sentence at install time —
/// pinned by [`tests::the_unbuilt_platforms_match_the_installers`]).
const UNBUILT_TARGETS: &[(&str, &str)] = &[(
    "x86_64-darwin",
    "Intel macOS is not built; use an Apple Silicon mac",
)];

/// [`host_target`], refused up front when the release line publishes no bundle for it.
///
/// Without this the failure surfaces from `fetch` as a bare `HTTP 404` on an asset
/// URL — indistinguishable from a broken release — several seconds and one download
/// attempt later. `install.sh` has always refused these before its first request;
/// `grove up` is the same decision on the same box.
pub(super) fn supported_host_target() -> Result<String, CliError> {
    let target = host_target();
    refuse_unbuilt(&target)?;
    Ok(target)
}

fn refuse_unbuilt(target: &str) -> Result<(), CliError> {
    match UNBUILT_TARGETS.iter().find(|(t, _)| *t == target) {
        Some((_, why)) => Err(CliError::Update(format!(
            "unsupported platform: {target} ({why})"
        ))),
        None => Ok(()),
    }
}

/// The host platform string used in bundle names, e.g. `aarch64-darwin`.
#[must_use]
pub fn host_target() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

pub(super) fn verify_sha256(bytes: &[u8], want_hex: &str) -> Result<(), CliError> {
    let got = hex(&Sha256::digest(bytes));
    if got == want_hex.to_lowercase() {
        Ok(())
    } else {
        Err(CliError::Update(format!(
            "checksum mismatch: expected {want_hex}, got {got}"
        )))
    }
}

pub(super) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The persisted channel a box is pinned to (`$GROVE_HOME/channel`), if any.
/// Written by `install.sh` so a canary box keeps pulling canaries without
/// re-specifying `--channel`.
pub(super) fn read_persisted_channel(home: &Path) -> Option<String> {
    let c = fs::read_to_string(home.join("channel")).ok()?;
    let c = c.trim();
    (!c.is_empty()).then(|| c.to_string())
}

/// A bearer token for the release repo, when the environment carries one —
/// `GH_TOKEN` then `GITHUB_TOKEN`, `gh`'s own precedence. Optional: the release repo
/// is public, so this only lifts GitHub's anonymous rate limit on resolution.
fn github_token() -> Option<String> {
    bearer(
        std::env::var("GH_TOKEN").ok(),
        std::env::var("GITHUB_TOKEN").ok(),
    )
}

/// The token-precedence rule, split out because a test may not set environment
/// variables: `set_var` is `unsafe` under edition 2024 and this workspace forbids
/// `unsafe_code`. Blank is not a token — an exported-but-empty `GH_TOKEN` (a CI
/// default that never got a value) must fall through, not become `Bearer `.
fn bearer(gh: Option<String>, github: Option<String>) -> Option<String> {
    [gh, github]
        .into_iter()
        .flatten()
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
}

/// A blocking HTTP client that (1) sends a `User-Agent` — api.github.com **403s**
/// requests without one — and (2) forces `Accept-Encoding: identity`, so a MITM
/// proxy that recompresses gzip mid-flight can't change the bytes out from under
/// the sha256 check (parity with `install.sh`).
///
/// **Redirects still follow — `resolve_stable_version` and GitHub's asset download
/// both need them — but every hop is re-checked against [`install_source`]'s rule.**
/// Classifying the base string once was not the control it was documented as: the
/// guard constrained the spelling of the URL, not where the bytes came from, so a
/// loopback base could 302 the fetch to any host over cleartext, and an `https` base
/// could be downgraded to `http` on redirect (reqwest permits a scheme change).
///
/// `auth` is attached only by the github.com call sites (see [`github_token`]);
/// nothing built from `GROVE_INSTALL_BASE_URL` ever gets one, so a mirror base can
/// never harvest the operator's token.
fn http_client(auth: Option<&str>) -> Result<reqwest::blocking::Client, CliError> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("grove/", env!("CARGO_PKG_VERSION")))
        .default_headers(default_headers(auth))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            if hop_is_allowed(attempt.url()) {
                return attempt.follow();
            }
            let refusal = format!(
                "refusing a redirect to {}: a self-update may not follow cleartext \
                 off loopback",
                attempt.url()
            );
            attempt.error(refusal)
        }))
        .build()
        .map_err(|e| CliError::Update(format!("build http client: {e}")))
}

/// May a redirect land here? `https` anywhere; plain `http` only on loopback — the
/// same rule [`install_source`] applies to the base, applied to each hop so the
/// promise it is documented with ("a MITM cannot redirect a self-update over
/// cleartext") is actually the property enforced.
fn hop_is_allowed(url: &reqwest::Url) -> bool {
    match url.scheme() {
        "https" => true,
        // `Url::host_str` keeps IPv6 brackets (`[::1]`), which the host token does
        // not carry — same peel `install_source` does on the raw authority.
        "http" => {
            let host = url.host_str().unwrap_or("");
            let host = host
                .strip_prefix('[')
                .map_or(host, |b| b.split(']').next().unwrap_or_default());
            url.username().is_empty() && is_loopback_host(host)
        }
        _ => false,
    }
}

/// The header set [`http_client`] installs, split out so it is assertable — a
/// `Client` exposes none of its defaults once built, and both of these headers are
/// load-bearing rather than decorative.
fn default_headers(auth: Option<&str>) -> reqwest::header::HeaderMap {
    use reqwest::header::{ACCEPT_ENCODING, AUTHORIZATION, HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if let Some(token) = auth
        && let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        // Sensitive headers are redacted by `Debug` and excluded from HPACK's
        // shared index — the token must not surface in a log line or a proxy cache.
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    headers
}

/// Hard ceiling on a downloaded bundle. A body at or under this is read whole; a
/// larger one is an error (not a silent truncation that later misreports as a
/// checksum mismatch).
const MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;

/// The same ceiling for the *text* fetches — the sha256 sidecar (65 bytes), the flat
/// sources' `latest` file (a version string), and one page of GitHub's releases list.
/// Generous beside what any of them can legitimately be, and still a ceiling: without
/// one, a hostile or merely broken mirror at `GROVE_INSTALL_BASE_URL` streams an
/// unbounded body into memory on a file the code expects to be a line long, and the
/// sidecar's 64-hex-digit check never gets to run because it runs *after* the read.
const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;

fn http_get_bytes(url: &str, auth: Option<&str>) -> Result<Vec<u8>, CliError> {
    let resp = http_client(auth)?
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| CliError::Update(format!("GET {url}: {e}")))?;
    read_capped(resp, MAX_BUNDLE_BYTES, url)
}

/// Read a body fully, but refuse one that exceeds `cap`. Reading `cap + 1` bytes
/// distinguishes "exactly at the limit" (ok) from "over" (error), so an oversized
/// download surfaces as a clear size error rather than truncated bytes that fail
/// the sha256 check with a misleading message.
fn read_capped(reader: impl Read, cap: u64, what: &str) -> Result<Vec<u8>, CliError> {
    let mut buf = Vec::new();
    reader
        .take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(|e| CliError::Update(format!("read {what}: {e}")))?;
    if buf.len() as u64 > cap {
        return Err(CliError::Update(format!(
            "{what}: bundle exceeds size limit ({cap} bytes)"
        )));
    }
    Ok(buf)
}

/// A text body, capped like the bundle is. `Response::text` would read to the end of
/// whatever arrives, so the cap — not the caller's expectations about the file — is
/// what bounds the allocation.
fn http_get_text(url: &str, auth: Option<&str>) -> Result<String, CliError> {
    http_get_text_capped(url, auth, MAX_TEXT_BYTES)
}

fn http_get_text_capped(url: &str, auth: Option<&str>, cap: u64) -> Result<String, CliError> {
    let resp = http_client(auth)?
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| CliError::Update(format!("GET {url}: {e}")))?;
    let bytes = read_capped(resp, cap, url)?;
    String::from_utf8(bytes).map_err(|e| CliError::Update(format!("{url} is not UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use tempfile::TempDir;

    #[test]
    fn install_source_refuses_remote_plain_http() {
        assert!(matches!(
            install_source(None).unwrap(),
            BundleSource::GitHubReleases { .. }
        ));
        assert!(matches!(
            install_source(Some("https://dl.example.com/x".into())).unwrap(),
            BundleSource::BaseUrl(_)
        ));
        // Loopback plain-http is the test seam — allowed.
        assert!(matches!(
            install_source(Some("http://127.0.0.1:8080/x".into())).unwrap(),
            BundleSource::BaseUrl(_)
        ));
        assert!(matches!(
            install_source(Some("http://localhost/x".into())).unwrap(),
            BundleSource::BaseUrl(_)
        ));
        // Bracketed IPv6 loopback is the seam too (host token is `::1`, not `[`).
        assert!(matches!(
            install_source(Some("http://[::1]:8080/x".into())).unwrap(),
            BundleSource::BaseUrl(_)
        ));
        assert!(matches!(
            install_source(Some("http://[::1]/x".into())).unwrap(),
            BundleSource::BaseUrl(_)
        ));
        // A remote plain-http base is refused outright.
        let err = install_source(Some("http://dl.example.com/x".into())).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        assert!(err.to_string().contains("plain-http"));
        // Userinfo can smuggle a remote host past a naive parse
        // (`127.0.0.1@evil.com` targets evil.com) — any '@' authority is refused,
        // including a nominally-loopback real host.
        for smuggled in [
            "http://127.0.0.1@evil.com/x",
            "http://[::1]@evil.com/x",
            "http://user@127.0.0.1:8080/x",
        ] {
            let err = install_source(Some(smuggled.into())).unwrap_err();
            assert!(err.to_string().contains("plain-http"), "{smuggled}");
        }
        // A non-URL value is a local fixture dir.
        assert!(matches!(
            install_source(Some("/tmp/fixture".into())).unwrap(),
            BundleSource::LocalDir(_)
        ));
    }

    /// The base-string rule applies to every REDIRECT hop too. A one-time check on
    /// the spelling of the base was not the control it was documented as: redirects
    /// follow by default, so a loopback base could 302 the bundle fetch to any host
    /// over cleartext, and an https base could be downgraded to http mid-chain.
    #[test]
    fn a_redirect_hop_obeys_the_same_transport_rule_as_the_base() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(hop_is_allowed(&url("https://github.com/o/r")));
        assert!(hop_is_allowed(&url(
            "https://objects.githubusercontent.com/x"
        )));
        assert!(hop_is_allowed(&url("http://127.0.0.1:8080/x")));
        assert!(hop_is_allowed(&url("http://localhost/x")));
        assert!(hop_is_allowed(&url("http://[::1]:8080/x")));

        assert!(!hop_is_allowed(&url("http://example.com/pwned")));
        assert!(
            !hop_is_allowed(&url("http://127.0.0.1@evil.com/x")),
            "userinfo smuggling is refused on a hop as on the base"
        );
        assert!(!hop_is_allowed(&url("file:///etc/passwd")));
    }

    /// The default source is this repo's own Releases — v1 published to a separate
    /// download satellite, which retires with the rewrite.
    #[test]
    fn the_release_repo_is_this_one() {
        let BundleSource::GitHubReleases { repo } = install_source(None).unwrap() else {
            panic!("no override → GitHub Releases");
        };
        assert_eq!(repo, "jgeschwendt/grove");
    }

    #[test]
    fn channel_resolution_paginates_until_a_match() {
        // A full first page of non-matching (stable) tags, then a canary on page 2.
        let page1 = {
            let tags: Vec<_> = (0..RELEASES_PER_PAGE)
                .map(|i| serde_json::json!({ "tag_name": format!("v0.0.{i}") }))
                .collect();
            serde_json::to_string(&tags).unwrap()
        };
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let got = resolve_channel_paged("canary", |page| {
            c.set(c.get() + 1);
            Ok(if page == 1 {
                page1.clone()
            } else {
                r#"[{"tag_name":"v0.2.0-canary.3"}]"#.to_string()
            })
        })
        .unwrap();
        assert_eq!(got, "0.2.0-canary.3");
        assert_eq!(calls.get(), 2, "walked to the second page");
    }

    #[test]
    fn channel_resolution_stops_on_a_short_page() {
        // A short first page with no match ends pagination (no endless fetching).
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let err = resolve_channel_paged("canary", |_page| {
            c.set(c.get() + 1);
            Ok("[]".to_string())
        });
        assert!(err.is_err());
        assert_eq!(calls.get(), 1);
    }

    /// The cap is not only the tarball's: the sidecar, the `latest` file and the
    /// releases-list JSON all arrive through [`http_get_text`], and each is read whole
    /// into memory *before* the validation that would reject it (the sha256 sidecar's
    /// 64-hex-digit check runs on an already-buffered body). A mirror that streams
    /// without end must hit a ceiling rather than the allocator.
    #[test]
    fn a_text_body_over_the_cap_is_refused_rather_than_buffered() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let _ = s.read(&mut [0u8; 1024]);
                let body = "x".repeat(100);
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        let url = format!("http://{addr}/latest");

        let err = http_get_text_capped(&url, None, 10).unwrap_err();
        assert!(err.to_string().contains("exceeds size limit"), "{err}");
        // …and a body inside the cap still reads whole.
        assert_eq!(http_get_text_capped(&url, None, 1000).unwrap().len(), 100);
    }

    #[test]
    fn read_capped_errors_when_over_the_limit() {
        let data = [0u8; 100];
        // Over the cap → a clear size error, not a truncated read.
        let err = read_capped(&data[..], 10, "bundle").unwrap_err();
        assert!(err.to_string().contains("exceeds size limit"));
        // Exactly at, and under, the cap read whole.
        assert_eq!(read_capped(&data[..], 100, "bundle").unwrap().len(), 100);
        assert_eq!(read_capped(&data[..], 1000, "bundle").unwrap().len(), 100);
    }

    #[test]
    fn host_target_is_arch_dash_os() {
        let t = host_target();
        assert!(t.contains('-'), "looks like arch-os: {t}");
        assert!(!t.contains("macos"), "macos normalised to darwin: {t}");
    }

    /// A platform the release matrix does not build is refused before a byte moves —
    /// with the reason, rather than as a 404 on an asset URL that was never published.
    #[test]
    fn an_unbuilt_platform_is_refused_up_front() {
        let err = refuse_unbuilt("x86_64-darwin").unwrap_err();
        assert_eq!(err.exit_code(), 7);
        assert!(err.to_string().contains("unsupported platform"), "{err}");
        assert!(err.to_string().contains("Apple Silicon"), "{err}");
        for built in ["aarch64-darwin", "x86_64-linux", "aarch64-linux"] {
            assert!(refuse_unbuilt(built).is_ok(), "{built} is in the matrix");
        }
    }

    /// The resolvers in this file follow `releases/latest` and then fetch that tag's
    /// assets, so a *published* release with no assets on it is not a slow release —
    /// it is a broken one, for every box on every platform, for as long as it stands.
    /// The workflow closes that window by shape rather than by timing: the release is
    /// created as a draft (invisible to `releases/latest` and to the asset listing)
    /// and undrafted only by a job that needs the whole bundle matrix.
    #[test]
    fn the_release_line_publishes_nothing_a_resolver_cannot_fetch() {
        let workflow = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.github/workflows/release.yml"
        ))
        .expect(".github/workflows/release.yml is readable from the crate");

        assert!(
            workflow.contains("gh release create") && workflow.contains("--draft "),
            "the release must be created as a draft, or `releases/latest` resolves it \
             while its bundles are still compiling"
        );
        let (_, promote) = workflow
            .split_once("--draft=false")
            .expect("some job must undraft the release, or nothing is ever published");
        assert!(
            workflow[..workflow.len() - promote.len()].contains("needs: [bundle]"),
            "the undraft job must depend on the whole bundle matrix — a partial \
             matrix must never promote"
        );
        // And what gets built must be what gets stamped and uploaded: on a
        // workflow_dispatch, the default checkout ref is the *branch* the dispatch
        // fired from, not the tag every other step keys off.
        assert!(
            workflow.contains("ref: ${{ github.event.inputs.tag || github.ref }}"),
            "the bundle job must check out the tag it stamps and uploads"
        );
    }

    /// The refusal exists in two places — here and `scripts/install.sh`, which runs
    /// before this binary exists on the box — and they must not drift: an installer
    /// that lets a platform through onto a `grove up` that refuses it is a box with no
    /// working update path.
    #[test]
    fn the_unbuilt_platforms_match_the_installers() {
        let installer = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/install.sh"
        ))
        .expect("scripts/install.sh is readable from the crate");
        let workflow = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.github/workflows/release.yml"
        ))
        .expect(".github/workflows/release.yml is readable from the crate");

        for (target, _) in UNBUILT_TARGETS {
            assert!(
                installer.contains(&format!("{target}) die \"unsupported platform")),
                "scripts/install.sh must refuse {target} up front too"
            );
            assert!(
                !workflow.contains(&format!("platform: {target}")),
                "{target} is in the release matrix, so it is built — drop it from \
                 UNBUILT_TARGETS"
            );
        }
    }

    #[test]
    fn stable_tag_parsed_off_the_redirect_url() {
        // The `releases/latest` redirect lands on the tag page; strip `/tag/` + `v`.
        assert_eq!(
            tag_from_release_url("https://github.com/jgeschwendt/grove/releases/tag/v0.1.0")
                .as_deref(),
            Some("0.1.0")
        );
        // Tolerate a trailing slash / query the redirect might carry.
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases/tag/v1.2.3/").as_deref(),
            Some("1.2.3")
        );
        // A non-tag URL (e.g. the releases index, if `latest` ever 200s in place).
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases"),
            None
        );
    }

    #[test]
    fn channel_tag_match_requires_a_numeric_suffix() {
        assert!(channel_tag_matches("v0.1.1-canary.2", "-canary."));
        assert!(channel_tag_matches("v0.1.0-canary.30", "-canary."));
        // Wrong channel, missing number, and non-numeric tail are all rejected.
        assert!(!channel_tag_matches("v0.1.1-beta.2", "-canary."));
        assert!(!channel_tag_matches("v0.1.1-canary", "-canary."));
        assert!(!channel_tag_matches("v0.1.1-canary.x", "-canary."));
        assert!(!channel_tag_matches("v0.1.1", "-canary."));
    }

    #[test]
    fn select_channel_picks_highest_and_tie_breaks_numerically() {
        // Mirrors a `GET /repos/<repo>/releases` body: newest-first, mixed channels.
        let body = r#"[
            {"tag_name": "v0.1.1-canary.1"},
            {"tag_name": "v0.1.0-canary.30"},
            {"tag_name": "v0.1.0"},
            {"tag_name": "v0.1.1-beta.5"}
        ]"#;
        // v0.1.1-canary.1 must beat v0.1.0-canary.30 (minor bump > higher canary N).
        assert_eq!(
            select_channel_version(body, "canary").unwrap(),
            "0.1.1-canary.1"
        );
        // Distinct channel with no matches → a clear error, not a wrong pick.
        assert!(select_channel_version(body, "nightly").is_err());
    }

    #[test]
    fn select_channel_errors_on_empty_list() {
        assert!(select_channel_version("[]", "canary").is_err());
    }

    #[test]
    fn channel_version_localdir_ignores_channel() {
        // The flat LocalDir source carries a single `latest` file, channel-agnostic.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("latest"), "0.4.0\n").unwrap();
        let src = BundleSource::LocalDir(dir.path().to_path_buf());
        assert_eq!(src.channel_version("stable").unwrap(), "0.4.0");
        assert_eq!(src.channel_version("canary").unwrap(), "0.4.0");
    }

    #[test]
    fn persisted_channel_read_from_home() {
        let home = TempDir::new().unwrap();
        assert_eq!(read_persisted_channel(home.path()), None);
        fs::write(home.path().join("channel"), "canary\n").unwrap();
        assert_eq!(
            read_persisted_channel(home.path()).as_deref(),
            Some("canary")
        );
        // An empty file is treated as unset, not the empty channel.
        fs::write(home.path().join("channel"), "  \n").unwrap();
        assert_eq!(read_persisted_channel(home.path()), None);
    }

    /// `GH_TOKEN` wins over `GITHUB_TOKEN`, and an exported-but-blank value is not a
    /// token — a CI environment that defines `GITHUB_TOKEN=""` must resolve
    /// anonymously rather than send `Authorization: Bearer `.
    #[test]
    fn a_blank_token_is_no_token() {
        assert_eq!(
            bearer(Some("gh".into()), Some("actions".into())).as_deref(),
            Some("gh")
        );
        assert_eq!(
            bearer(None, Some("actions".into())).as_deref(),
            Some("actions")
        );
        assert_eq!(bearer(Some("  ".into()), None), None);
        assert_eq!(bearer(None, None), None);
    }

    /// The client carries the hardening the sha256 check depends on:
    /// `Accept-Encoding: identity`, so a proxy that recompresses mid-flight cannot
    /// change the bytes being hashed. A token, when present, rides as a *sensitive*
    /// bearer header — and when absent, no `Authorization` is sent at all, which is
    /// what keeps an anonymous fetch anonymous.
    #[test]
    fn the_client_pins_its_headers_and_hides_a_token() {
        use reqwest::header::{ACCEPT_ENCODING, AUTHORIZATION};

        let anonymous = default_headers(None);
        assert_eq!(anonymous[ACCEPT_ENCODING], "identity");
        assert!(!anonymous.contains_key(AUTHORIZATION));

        let authed = default_headers(Some("secret"));
        assert_eq!(authed[AUTHORIZATION], "Bearer secret");
        assert!(
            authed[AUTHORIZATION].is_sensitive(),
            "a token must never be printable by Debug"
        );
        assert!(http_client(Some("secret")).is_ok());
    }
}
