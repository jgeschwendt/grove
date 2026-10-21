//! Release resolution + HTTP fetch: where update bundles come from (GitHub
//! Releases in production; flat `LocalDir`/`BaseUrl` for the smoke tests).
//!
//! v2 publishes to **this repo's own Releases** rather than v1's separate download
//! satellite. The repo is public, so both halves work anonymously: channel resolution
//! reads the releases list, and the bundle comes off the plain `releases/download/…`
//! URL. A `GH_TOKEN`/`GITHUB_TOKEN` in the environment rides the *resolution* calls
//! only — to the [`GitHub`] hosts, which only `install_source(None)` fills — purely to
//! lift GitHub's anonymous rate limit; nothing here requires one.
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

use reqwest::blocking::{Client, Response};
use sha2::{Digest, Sha256};

use super::layout::version_key;
use crate::CliError;

/// Page size and page cap for walking the GitHub releases list when resolving a
/// prerelease channel (a canary can sit past the first page).
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 20;

/// GitHub repo the release channels resolve against — where `install.sh` and
/// `release.yaml` publish. Overridable for tests via `GROVE_INSTALL_BASE_URL` (an http
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
        return Ok(BundleSource::GitHubReleases(GitHub {
            repo: RELEASE_REPO.to_string(),
            web: "https://github.com".to_string(),
            api: "https://api.github.com".to_string(),
        }));
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
#[derive(Debug, PartialEq)]
pub enum BundleSource {
    GitHubReleases(GitHub),
    LocalDir(PathBuf),
    BaseUrl(String),
}

/// A release repo and the two hosts it answers on. The fields are private so that
/// only this module decides where a token goes: these hosts get one, and nothing
/// spelled by `GROVE_INSTALL_BASE_URL` can become one of them.
#[derive(Debug, PartialEq)]
pub struct GitHub {
    repo: String,
    web: String,
    api: String,
}

impl BundleSource {
    /// Resolve the version a `channel` currently points at. For the flat
    /// `LocalDir`/`BaseUrl` schemes the channel is ignored (they carry a single
    /// `latest` file — the smoke tests).
    pub fn channel_version(&self, channel: &str) -> Result<String, CliError> {
        let latest = match self {
            BundleSource::LocalDir(dir) => fs::read_to_string(dir.join("latest"))
                .map_err(|e| CliError::Update(format!("read latest: {e}")))?,
            BundleSource::BaseUrl(base) => {
                get_text(&http_client(None)?, &format!("{base}/latest"))?
            }
            BundleSource::GitHubReleases(gh) => return gh.channel_version(channel),
        };
        Ok(latest.trim().to_string())
    }

    /// Fetch the bundle for `version`/`target` and return its bytes, verified against
    /// the sha256 in the sidecar `.sha256` file.
    pub(super) fn fetch(&self, version: &str, target: &str) -> Result<Vec<u8>, CliError> {
        let rel = format!("{version}/{target}.tar.gz");
        let verified = |tar: Vec<u8>, sidecar: String| {
            // The sidecar may be `<hex>` or `<hex>  filename`; take the first field.
            let want = sidecar
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(CliError::Update(format!(
                    "{rel}.sha256 is not a sha256 hex digest"
                )));
            }
            let got = format!("{:x}", Sha256::digest(&tar));
            (got == want).then_some(tar).ok_or_else(|| {
                CliError::Update(format!("checksum mismatch: expected {want}, got {got}"))
            })
        };
        let url = match self {
            BundleSource::LocalDir(dir) => {
                let read = |p: &str| {
                    fs::read(dir.join(p)).map_err(|e| CliError::Update(format!("read {p}: {e}")))
                };
                let tar = read(&rel)?;
                let sidecar = read(&format!("{rel}.sha256"))?;
                return verified(tar, String::from_utf8_lossy(&sidecar).into_owned());
            }
            BundleSource::BaseUrl(base) => format!("{base}/{rel}"),
            // The public asset URL. A token is deliberately NOT sent here: the
            // download 302s to a storage host, and a credential must not follow a
            // redirect off github.com. Public assets need none anyway.
            BundleSource::GitHubReleases(gh) => format!(
                "{}/{}/releases/download/v{version}/{target}.tar.gz",
                gh.web, gh.repo
            ),
        };
        let client = http_client(None)?;
        let tar = get(&client, &url, MAX_BUNDLE_BYTES)?;
        verified(tar, get_text(&client, &format!("{url}.sha256"))?)
    }
}

impl GitHub {
    /// `stable` follows `<web>/<repo>/releases/latest` — a redirect to the tag page,
    /// and one that excludes prereleases — and reads the version off the final URL.
    /// Any other channel is the highest `v*-<channel>.N` prerelease (mirrors
    /// `scripts/install.sh`), walked page by page: releases come newest-first, so the
    /// highest of a channel lives on the first page that carries the channel at all,
    /// and a short page (< `RELEASES_PER_PAGE`) is the last one.
    fn channel_version(&self, channel: &str) -> Result<String, CliError> {
        let Self { repo, web, api } = self;
        let client = http_client(github_token().as_deref())?;
        if channel == "stable" {
            let resp = send(&client, &format!("{web}/{repo}/releases/latest"))?;
            let url = resp.url().as_str();
            return tag_from_release_url(url).ok_or_else(|| {
                CliError::Update(format!("could not resolve latest stable release ({url})"))
            });
        }
        for page in 1..=MAX_RELEASE_PAGES {
            let body = get_text(
                &client,
                &format!("{api}/repos/{repo}/releases?per_page={RELEASES_PER_PAGE}&page={page}"),
            )?;
            match page_best(&body, channel)? {
                (Some(v), _) => return Ok(v),
                (None, n) if n < RELEASES_PER_PAGE => break,
                _ => {}
            }
        }
        Err(CliError::Update(format!(
            "no {channel} releases found on {repo}"
        )))
    }
}

/// Version from a `.../releases/tag/<tag>` URL (the `releases/latest` redirect
/// target), leading `v` stripped. `None` if the URL isn't a tag page.
fn tag_from_release_url(url: &str) -> Option<String> {
    let (_, tag) = url.rsplit_once("/tag/")?;
    let tag = tag.split(['?', '#']).next()?.trim_end_matches('/');
    (!tag.is_empty()).then(|| tag.trim_start_matches('v').to_string())
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
        .max_by_key(|t| version_key(t))
        .map(|tag| tag.trim_start_matches('v').to_string());
    Ok((best, releases.len()))
}

/// A tag belongs to the channel iff `<suffix>` (`-<channel>.`) is present and
/// followed by a non-empty run of digits to the end (`v0.1.1-canary.2` ✓;
/// `v0.1.1-canary` ✗; `v0.1.1-beta.2` ✗ for `-canary.`).
fn channel_tag_matches(tag: &str, suffix: &str) -> bool {
    tag.rsplit_once(suffix)
        .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Platforms this box could ask for and the release line does not publish, with the
/// reason an operator needs. Kept in step with two other lists by construction: the
/// matrix in `.github/workflows/release.yaml` (which builds the bundles) and the
/// refusal in `scripts/install.sh` (which says the same sentence at install time —
/// pinned by [`tests::the_repo_files_agree_with_the_resolvers`]).
const UNBUILT_TARGETS: &[(&str, &str)] = &[(
    "x86_64-darwin",
    "Intel macOS is not built; use an Apple Silicon mac",
)];

/// `target`, refused up front when the release line publishes no bundle for it.
///
/// Without this the failure surfaces from `fetch` as a bare `HTTP 404` on an asset
/// URL — indistinguishable from a broken release — several seconds and one download
/// attempt later. `install.sh` has always refused these before its first request;
/// `grove up` is the same decision on the same box.
pub(super) fn supported(target: String) -> Result<String, CliError> {
    match UNBUILT_TARGETS.iter().find(|(t, _)| *t == target) {
        Some((_, why)) => Err(CliError::Update(format!(
            "unsupported platform: {target} ({why})"
        ))),
        None => Ok(target),
    }
}

/// The host platform string used in bundle names, e.g. `aarch64-darwin`.
#[must_use]
pub fn host_target() -> String {
    format!(
        "{}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS.replace("macos", "darwin")
    )
}

/// The persisted channel a box is pinned to (`$GROVE_INSTALL/channel`), if any.
/// Written by `install.sh` so a canary box keeps pulling canaries without
/// re-specifying `--channel`.
pub(super) fn read_persisted_channel(install: &Path) -> Option<String> {
    let c = fs::read_to_string(install.join("channel")).ok()?;
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
/// **Redirects still follow — `releases/latest` and GitHub's asset download both
/// need them — but every hop is re-checked against [`install_source`]'s rule.**
/// Classifying the base string once was not the control it was documented as: the
/// guard constrained the spelling of the URL, not where the bytes came from, so a
/// loopback base could 302 the fetch to any host over cleartext, and an `https` base
/// could be downgraded to `http` on redirect (reqwest permits a scheme change).
///
/// `auth` is attached only by [`GitHub::channel_version`], whose hosts only
/// `install_source(None)` fills (see [`github_token`]); nothing built from
/// `GROVE_INSTALL_BASE_URL` ever gets one, so a mirror base can never harvest the
/// operator's token, and no asset download carries one.
fn http_client(auth: Option<&str>) -> Result<Client, CliError> {
    Client::builder()
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

/// `GET url`, non-2xx an error. reqwest's `Display` for a redirect error stops at
/// "error following redirect for url (…)" and keeps the policy's reason — the
/// refusal, or "too many redirects" — in `source()`, so that is appended.
fn send(client: &Client, url: &str) -> Result<Response, CliError> {
    client
        .get(url)
        .send()
        .and_then(Response::error_for_status)
        .map_err(|e| {
            let why = std::error::Error::source(&e)
                .map(|s| format!(": {s}"))
                .unwrap_or_default();
            CliError::Update(format!("GET {url}: {e}{why}"))
        })
}

/// A body read whole, but refused past `cap`. Reading `cap + 1` bytes distinguishes
/// "exactly at the limit" (ok) from "over" (error), so an oversized download surfaces
/// as a clear size error rather than truncated bytes that fail the sha256 check with
/// a misleading message.
fn get(client: &Client, url: &str, cap: u64) -> Result<Vec<u8>, CliError> {
    let mut buf = Vec::new();
    send(client, url)?
        .take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(|e| CliError::Update(format!("read {url}: {e}")))?;
    if buf.len() as u64 > cap {
        return Err(CliError::Update(format!(
            "{url}: bundle exceeds size limit ({cap} bytes)"
        )));
    }
    Ok(buf)
}

/// A text body, capped like the bundle is. `Response::text` would read to the end of
/// whatever arrives, so the cap — not the caller's expectations about the file — is
/// what bounds the allocation.
fn get_text(client: &Client, url: &str) -> Result<String, CliError> {
    String::from_utf8(get(client, url, MAX_TEXT_BYTES)?)
        .map_err(|e| CliError::Update(format!("{url} is not UTF-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use tempfile::TempDir;

    /// A loopback server answering every request through `route`: `Ok(body)` is a
    /// 200, `Err(location)` a 302. One connection per request.
    fn serve(route: impl Fn(&str) -> Result<Vec<u8>, String> + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut req = [0u8; 4096];
                let n = s.read(&mut req).unwrap_or(0);
                let head = String::from_utf8_lossy(&req[..n]);
                let (status, body) = match route(head.split(' ').nth(1).unwrap_or("")) {
                    Ok(body) => ("200 OK".to_string(), body),
                    Err(to) => (format!("302 Found\r\nlocation: {to}"), Vec::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(&[head.as_bytes(), &body].concat());
            }
        });
        base
    }

    /// Both remote sources end to end over loopback: resolution (the flat `latest`,
    /// GitHub's stable redirect and its paged prerelease walk), the verified fetch, and
    /// every transport refusal — the text cap (a mirror that streams without end must
    /// hit a ceiling rather than the allocator, *before* the sidecar's hex check runs),
    /// strict UTF-8, and the redirect policy's reasons surfacing in the error.
    #[test]
    fn remote_sources_resolve_and_fetch_over_http() {
        let sidecar = format!("{:X}  t.tar.gz\n", Sha256::digest(b"bundle"));
        // A full first page of non-matching (stable) tags; the canary is on page 2,
        // among tags that must not match it.
        let page1: Vec<_> = (0..RELEASES_PER_PAGE)
            .map(|i| serde_json::json!({ "tag_name": format!("v0.0.{i}") }))
            .collect();
        let page1 = serde_json::to_vec(&page1).unwrap();
        let page2 = r#"[{"tag_name":"v0.1.0-canary.30"},{"tag_name":"v0.1.1-canary.1"},
            {"tag_name":"v0.9.0-canary"},{"tag_name":"v0.9.0-canary.x"},
            {"tag_name":"v0.9.0-beta.5"},{"tag_name":"v0.9.0"}]"#;
        let base = serve(move |p| match p {
            "/latest" => Ok("0.4.0\n".into()),
            "/o/r/releases/latest" => Err("/o/r/releases/tag/v1.2.3/?q".into()),
            "/off" => Err("http://example.com/x".into()),
            "/loop" => Err("/loop".into()),
            "/big" => Ok(vec![b'x'; 100]),
            "/binary" => Ok(vec![0xff]),
            p if p.ends_with("&page=1") => Ok(page1.clone()),
            p if p.ends_with("&page=2") => Ok(page2.into()),
            p if p.starts_with("/nohex") && p.ends_with(".sha256") => Ok("zz".into()),
            p if p.ends_with(".sha256") => Ok(sidecar.clone().into()),
            p if p.starts_with("/bad") => Ok("tampered".into()),
            _ => Ok("bundle".into()),
        });
        let gh = |repo: &str| {
            let (web, api) = (base.clone(), base.clone());
            BundleSource::GitHubReleases(GitHub {
                repo: repo.into(),
                web,
                api,
            })
        };
        let flat = BundleSource::BaseUrl(base.clone());
        let local = TempDir::new().unwrap();
        fs::write(local.path().join("latest"), "0.3.0\n").unwrap();

        assert_eq!(flat.channel_version("canary").unwrap(), "0.4.0");
        assert_eq!(
            BundleSource::LocalDir(local.path().into())
                .channel_version("x")
                .unwrap(),
            "0.3.0"
        );
        assert_eq!(gh("o/r").channel_version("stable").unwrap(), "1.2.3");
        assert_eq!(
            gh("o/r").channel_version("canary").unwrap(),
            "0.1.1-canary.1"
        );
        assert_eq!(flat.fetch("0.4.0", "t").unwrap(), b"bundle");
        assert_eq!(gh("o/r").fetch("0.4.0", "t").unwrap(), b"bundle");
        let client = http_client(None).unwrap();
        assert_eq!(
            get(&client, &format!("{base}/big"), 100).unwrap().len(),
            100
        );
        let missing = BundleSource::LocalDir(local.path().join("missing"));
        for (err, want) in [
            (missing.channel_version("x").unwrap_err(), "read latest"),
            (missing.fetch("1", "t").unwrap_err(), "read 1/t.tar.gz"),
            (
                gh("x/y").channel_version("stable").unwrap_err(),
                "could not resolve latest stable",
            ),
            (
                gh("o/r").channel_version("nightly").unwrap_err(),
                "no nightly releases found on o/r",
            ),
            (flat.fetch("bad", "t").unwrap_err(), "checksum mismatch"),
            (
                flat.fetch("nohex", "t").unwrap_err(),
                "is not a sha256 hex digest",
            ),
            (
                get(&client, &format!("{base}/big"), 99).unwrap_err(),
                "exceeds size limit",
            ),
            (
                get_text(&client, &format!("{base}/binary")).unwrap_err(),
                "is not UTF-8",
            ),
            (
                get(&client, &format!("{base}/off"), 9).unwrap_err(),
                "refusing a redirect to http://example.com/x",
            ),
            (
                get(&client, &format!("{base}/loop"), 9).unwrap_err(),
                "too many redirects",
            ),
        ] {
            assert!(err.to_string().contains(want), "{err}");
        }
    }

    /// Plain http is loopback-only, and the rule binds the base AND every redirect hop
    /// — a one-time check on the base's spelling let a loopback base 302 the fetch to
    /// any host over cleartext. Userinfo can smuggle a remote host past a naive parse
    /// (`127.0.0.1@evil.com` targets evil.com), so any '@' authority is refused.
    #[test]
    fn plain_http_is_loopback_only_on_the_base_and_every_hop() {
        for (url, ok) in [
            ("https://dl.example.com/x", true),
            ("http://127.0.0.1:8080/x", true),
            ("http://localhost/x", true),
            ("http://[::1]:8080/x", true),
            ("http://[::1]/x", true),
            ("http://dl.example.com/x", false),
            ("http://127.0.0.1@evil.com/x", false),
            ("http://[::1]@evil.com/x", false),
            ("http://user@127.0.0.1:8080/x", false),
        ] {
            match install_source(Some(url.into())) {
                Ok(src) => assert!(ok && src == BundleSource::BaseUrl(url.into()), "{url}"),
                Err(e) => assert!(!ok && e.to_string().contains("plain-http"), "{url}: {e}"),
            }
            assert_eq!(
                hop_is_allowed(&reqwest::Url::parse(url).unwrap()),
                ok,
                "{url}"
            );
        }
        assert!(!hop_is_allowed(
            &reqwest::Url::parse("file:///etc/passwd").unwrap()
        ));
        assert_eq!(
            install_source(Some("/tmp/fixture".into())).unwrap(),
            BundleSource::LocalDir("/tmp/fixture".into())
        );
        // The default is this repo's own Releases — v1 published to a separate
        // download satellite, which retires with the rewrite.
        assert_eq!(
            install_source(None).unwrap(),
            BundleSource::GitHubReleases(GitHub {
                repo: "jgeschwendt/grove".into(),
                web: "https://github.com".into(),
                api: "https://api.github.com".into(),
            })
        );
    }

    /// A platform the release matrix does not build is refused before a byte moves —
    /// with the reason, rather than as a 404 on an asset URL that was never published.
    #[test]
    fn an_unbuilt_platform_is_refused_up_front() {
        let err = supported("x86_64-darwin".into()).unwrap_err();
        assert_eq!(err.exit_code(), 7);
        assert!(err.to_string().contains("unsupported platform"), "{err}");
        assert!(err.to_string().contains("Apple Silicon"), "{err}");
        for built in ["aarch64-darwin", "x86_64-linux", "aarch64-linux"] {
            assert_eq!(supported(built.into()).unwrap(), built);
        }
        let host = host_target();
        assert!(
            host.contains('-') && !host.contains("macos"),
            "arch-os: {host}"
        );
    }

    /// Two repo files must agree with the resolvers here. The release workflow: a
    /// *published* release with no assets is not a slow release but a broken one, for
    /// every box, for as long as it stands — so it is created as a draft (invisible to
    /// `releases/latest`) and undrafted only by a job that needs the whole bundle
    /// matrix. And the unbuilt-platform refusal, which `scripts/install.sh` repeats
    /// before this binary exists on the box: an installer that lets a platform through
    /// onto a `grove up` that refuses it is a box with no working update path.
    #[test]
    fn the_repo_files_agree_with_the_resolvers() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../");
        let workflow = fs::read_to_string(format!("{root}.github/workflows/release.yaml"))
            .expect(".github/workflows/release.yaml is readable from the crate");
        let installer = fs::read_to_string(format!("{root}scripts/install.sh"))
            .expect("scripts/install.sh is readable from the crate");

        assert!(
            workflow.contains("gh release create") && workflow.contains("--draft "),
            "the release must be created as a draft, or `releases/latest` resolves it \
             while its bundles are still compiling"
        );
        let (before, _) = workflow
            .split_once("--draft=false")
            .expect("some job must undraft the release, or nothing is ever published");
        assert!(
            before.contains("needs: [bundle]"),
            "the undraft job must depend on the whole bundle matrix — a partial \
             matrix must never promote"
        );
        // What gets built must be what gets stamped and uploaded: on a
        // workflow_dispatch, the default checkout ref is the *branch* the dispatch
        // fired from, not the tag every other step keys off.
        assert!(
            workflow.contains("ref: ${{ github.event.inputs.tag || github.ref }}"),
            "the bundle job must check out the tag it stamps and uploads"
        );
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
    }
}
