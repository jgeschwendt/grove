#!/bin/bash
# grove installer.
#
# Deliberately thin. It resolves a release on jgeschwendt/grove, downloads the
# single-binary bundle + its checksum sidecar + a bootstrap `grove`, and hands off
# to `grove up`, which owns the versioned-dir install and the atomic `current`
# symlink flip. Every line of that is unit-tested Rust; the GitHub fetch is the only
# logic that lives in bash, and it stages into the same local-dir source
# (`GROVE_INSTALL_BASE_URL`) the smoke test and `grove up` already understand.
#
# Usage:
#   bash install.sh                     # latest stable
#   channel=canary bash install.sh      # latest prerelease of a channel
#   bash install.sh v0.1.0              # a pinned tag (with or without the v)
#
# Environment:
#   channel                 release channel: stable (default), canary, …
#   GROVE_INSTALL           install root — versions/, current, channel (default:
#                           $XDG_DATA_HOME/grove, i.e. ~/.local/share/grove).
#                           Disposable: another `grove up` regenerates all of it.
#   GROVE_HOME              workspace dir — manifest.toml, code/ (default: ~/.grove).
#                           Holds the checkouts, so nothing here is regenerable.
#   GROVE_LINK_DIR          where to put the `grove` PATH symlink. Unset, the script
#                           searches /usr/local/bin then ~/.local/bin — a search that
#                           can leave a redirected HOME, so set this whenever the run
#                           must stay inside one.
#   GH_TOKEN / GITHUB_TOKEN optional GitHub token — rate limit only; see below
#   GROVE_INSTALL_BASE_URL  skip GitHub entirely and install from this release base
#                           (a local directory or an http(s) base laid out as
#                           <base>/<version>/<target>.tar.gz + .sha256 + grove-<target>
#                           + grove-<target>.sha256, with <base>/latest naming the
#                           default version). This is what test/install_smoke.sh
#                           drives, so the smoke exercises this script rather than a
#                           parallel copy of it. A plain-http base is refused unless
#                           it is loopback, mirroring `grove up`'s own rule.
#
# ── on tokens ───────────────────────────────────────────────────────────────────
# jgeschwendt/grove is public and its release assets are anonymously readable, so no
# credential is required: unset, this script resolves and downloads over the plain
# `releases/download/…` URLs. GH_TOKEN (or GITHUB_TOKEN) is an optional rate-limit
# helper — GitHub's anonymous API budget is per-IP and shared, which a busy CI runner
# can exhaust. With one exported the script switches to the REST API's asset endpoint,
# which spends the token's own budget instead. Same bundle, same checksums, either way.
# stele:landmark install-hands-off-to-grove-up
set -euo pipefail

repo="jgeschwendt/grove"
grove_home="${GROVE_HOME:-$HOME/.grove}"
grove_install="${GROVE_INSTALL:-${XDG_DATA_HOME:-$HOME/.local/share}/grove}"
channel="${channel:-stable}"
version="${1:-}"
base="${GROVE_INSTALL_BASE_URL:-}"
token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"

die() {
  echo "grove: $1" >&2
  exit 1
}

# Auth header as an array so an empty token adds no argument at all (an empty
# string one would send `Authorization:` and get a 401 instead of an anonymous 200).
# `${auth[@]+…}` and not a bare `"${auth[@]}"`: macOS ships bash 3.2, where
# expanding an EMPTY array under `set -u` is an unbound-variable fatal — the
# anonymous path would die before its first request.
auth=()
if [ -n "$token" ]; then auth=(-H "Authorization: Bearer $token"); fi

# Accept-Encoding: identity defeats corp MITM proxies that recompress gzip
# mid-flight — re-compression changes the bytes and the sha256 sidecar `grove up`
# verifies would no longer match. Mirrors the Rust client's own default header.
curl_opts=(-fsSL -H "Accept-Encoding: identity")

# Refuse a cleartext URL that is not loopback — the same rule `grove up` applies to
# GROVE_INSTALL_BASE_URL (crates/grove/src/update/source.rs::install_source),
# including its refusal of userinfo (`http://127.0.0.1@evil.com/x` targets evil.com).
# This script runs BEFORE any grove binary exists on the box, so the Rust guard
# cannot cover it: without this, a remote http base is downloaded and executed here
# and only refused afterwards, by the binary it already ran.
check_url() { # $1=url
  case "$1" in
  https://*) return 0 ;;
  http://*) ;;
  *) return 0 ;;
  esac
  authority="${1#http://}"
  authority="${authority%%/*}"
  case "$authority" in
  *@*) die "refusing plain-http URL with userinfo: $1" ;;
  esac
  case "$authority" in
  \[*\]*)
    host="${authority#\[}"
    host="${host%%\]*}"
    ;;
  *) host="${authority%%:*}" ;;
  esac
  case "$host" in
  localhost | 127.0.0.1 | ::1) return 0 ;;
  esac
  die "refusing plain-http URL $1: use https:// (a 127.0.0.1/localhost base is allowed for testing)"
}

# Read a URL or local file to stdout / copy one to a destination path. The local
# arm is what makes a fixture directory a first-class release base.
read_text() {
  case "$1" in
  http://* | https://*)
    check_url "$1"
    curl "${curl_opts[@]}" "$1"
    ;;
  *) cat "$1" ;;
  esac
}
fetch() {
  case "$1" in
  http://* | https://*)
    check_url "$1"
    curl "${curl_opts[@]}" "$1" -o "$2"
    ;;
  *) cp "$1" "$2" ;;
  esac
}

# The sha256 of a file, on either a coreutils or a BSD/macOS box.
sha256_of() { # $1=path
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# The bootstrap binary is the one asset this script EXECUTES, so it is the one that
# most needs checking — and it was the one that had no sidecar at all: the checksum
# `grove up` verifies covers the tarball, and it was verified BY this unchecked
# binary. Required, never best-effort; a missing sidecar is a broken release.
verify_bootstrap() { # $1=binary $2=expected-hex
  want=$(printf '%s' "$2" | tr -d '[:space:]')
  [ -n "$want" ] || die "no checksum published for grove-$target; refusing to run it"
  got=$(sha256_of "$1")
  [ "$got" = "$want" ] || die "bootstrap checksum mismatch for grove-$target (got $got, want $want)"
}

# Keep only the tags whose prerelease suffix is exactly `-<channel>.N`.
#
# A literal comparison, never a regex: `channel` is operator-supplied and
# interpolating it into an ERE lets a value like `[a-z]*` resolve — and then
# persist — a channel nobody asked for. `grove up` matches the suffix literally for
# the same reason (update::source).
channel_tags() { # $1=channel
  while IFS= read -r t; do
    case "$t" in *-*) ;; *) continue ;; esac
    suffix="${t##*-}"
    name="${suffix%.*}"
    n="${suffix##*.}"
    [ "$name" = "$1" ] || continue
    case "$n" in '' | *[!0-9]*) continue ;; esac
    printf '%s\n' "$t"
  done
}

gh_api() { curl "${curl_opts[@]}" ${auth[@]+"${auth[@]}"} -H "X-GitHub-Api-Version: 2022-11-28" "https://api.github.com/$1"; }

# The API asset URL for an asset NAME inside a release JSON body. Each asset object
# carries its `url` before its `name`, and its nested `uploader` object after, so
# splitting the body on `{` puts both fields — and nothing from a later asset — in
# one chunk. Used only on the token path; anonymous installs take the public
# download URL instead.
asset_url() {
  printf '%s' "$2" | tr '{' '\n' |
    grep -E "\"name\": ?\"$(printf '%s' "$1" | sed 's/[.]/\\./g')\"" |
    grep -oE 'https://api\.github\.com/repos/[^"]+/releases/assets/[0-9]+' | head -1
}

# Platform string, matching update::source::host_target(): <arch>-<os>.
case "$(uname -m)" in
arm64 | aarch64) arch=aarch64 ;;
x86_64 | amd64) arch=x86_64 ;;
*) die "unsupported architecture: $(uname -m)" ;;
esac
case "$(uname -s)" in
Darwin) os=darwin ;;
Linux) os=linux ;;
*) die "unsupported OS: $(uname -s)" ;;
esac
target="${arch}-${os}"

# The release matrix builds Apple Silicon only on macOS — reject an Intel mac up
# front with a clear "unsupported platform" rather than letting the fetch below fail
# with a misleading asset-unreachable error on a tag that never carried it.
case "$target" in
x86_64-darwin) die "unsupported platform: $target (Intel macOS is not built; use an Apple Silicon mac)" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -n "$base" ]; then
  # ── staged mode: a release base is already laid out for us ───────────────────
  if [ -n "$version" ]; then vsn="${version#v}"; else
    vsn=$(read_text "$base/latest" | tr -d '[:space:]')
  fi
  [ -n "$vsn" ] || die "could not resolve a version from $base/latest"
  echo "grove: installing ${vsn} (${target}) from ${base}"
  fetch "$base/$vsn/grove-$target" "$tmp/grove" || die "bootstrap CLI unreachable: grove-$target"
  bootstrap_sha=$(read_text "$base/$vsn/grove-$target.sha256") ||
    die "bootstrap checksum unreachable: grove-$target.sha256"
else
  # ── GitHub Releases ─────────────────────────────────────────────────────────
  if [ -n "$version" ]; then
    case "$version" in v*) tag="$version" ;; *) tag="v$version" ;; esac
  elif [ "$channel" = "stable" ]; then
    # `releases/latest` excludes prereleases — exactly the stable channel.
    tag=$(gh_api "repos/${repo}/releases/latest" |
      grep -oE '"tag_name": ?"[^"]+"' | sed -E 's/.*"(v?[^"]+)"$/\1/' | head -1)
    [ -n "$tag" ] || die "could not resolve the latest stable release on ${repo}"
  else
    # A channel is a prerelease tag suffix, v<semver>-<channel>.N. Numeric-aware
    # sort so v0.1.1-canary.1 outranks v0.1.0-canary.30, mirroring
    # update::layout::version_key.
    tag=$(gh_api "repos/${repo}/releases?per_page=100" |
      grep -oE '"tag_name": ?"v[^"]+"' |
      sed -E 's/.*"(v[^"]+)".*/\1/' |
      channel_tags "$channel" |
      sort -t. -k1,1V -k2,2V -k3,3V -k4,4n | tail -1)
    [ -n "$tag" ] || die "no ${channel} releases found on ${repo}"
  fi
  vsn="${tag#v}"

  # Stage the three files as a local-dir source: `grove up` reads
  # <base>/<vsn>/<target>.tar.gz (+ .sha256), so lay them out exactly that way.
  base="$tmp/stage"
  mkdir -p "$base/$vsn"
  echo "grove: installing ${vsn} (${target}) from ${repo}"

  if [ -n "$token" ]; then
    release=$(gh_api "repos/${repo}/releases/tags/${tag}") ||
      die "release ${tag} unreachable on ${repo} (is the token scoped to contents: read?)"
    for asset in "$target.tar.gz" "$target.tar.gz.sha256" "grove-$target" "grove-$target.sha256"; do
      url=$(asset_url "$asset" "$release")
      [ -n "$url" ] || die "release asset missing on ${tag}: ${asset}"
      case "$asset" in
      grove-*.sha256) dest="$tmp/grove.sha256" ;;
      grove-*) dest="$tmp/grove" ;;
      *) dest="$base/$vsn/$asset" ;;
      esac
      curl "${curl_opts[@]}" ${auth[@]+"${auth[@]}"} -H "Accept: application/octet-stream" "$url" -o "$dest" ||
        die "release asset unreachable: ${asset}"
    done
    bootstrap_sha=$(cat "$tmp/grove.sha256")
  else
    dl="https://github.com/${repo}/releases/download/${tag}"
    fetch "$dl/$target.tar.gz" "$base/$vsn/$target.tar.gz" ||
      die "release asset unreachable: $target.tar.gz (rate-limited? export GH_TOKEN)"
    fetch "$dl/$target.tar.gz.sha256" "$base/$vsn/$target.tar.gz.sha256" ||
      die "release asset unreachable: $target.tar.gz.sha256"
    fetch "$dl/grove-$target" "$tmp/grove" ||
      die "bootstrap CLI unreachable: grove-$target"
    bootstrap_sha=$(read_text "$dl/grove-$target.sha256") ||
      die "bootstrap checksum unreachable: grove-$target.sha256"
  fi
fi

# Before `chmod +x`, and before it runs: this is the only asset the script executes.
verify_bootstrap "$tmp/grove" "$bootstrap_sha"
chmod +x "$tmp/grove"

# Hand off: the bootstrap CLI does the versioned-dir install, the checksum
# verification and the atomic flip. On a clean machine nothing is running, so this
# only lays versions/<vsn> and moves `current`.
GROVE_HOME="$grove_home" GROVE_INSTALL="$grove_install" GROVE_INSTALL_BASE_URL="$base" \
  "$tmp/grove" up --version "$vsn"

# Persist the channel this box follows so a later `grove up` (no --channel /
# GROVE_CHANNEL) keeps pulling from it. Derived from the *resolved version*, not
# $channel: a pinned `install.sh v0.1.1-canary.2` runs with channel=stable (the
# default) but is really a canary box, and keying off $channel would strand it on
# stable. A prerelease suffix (-<channel>.N) ⇒ that channel; a bare semver ⇒ stable.
case "$vsn" in
*-*.[0-9]*)
  resolved_channel="${vsn##*-}"
  resolved_channel="${resolved_channel%.*}"
  ;;
*) resolved_channel="stable" ;;
esac
mkdir -p "$grove_install"
printf '%s\n' "$resolved_channel" >"$grove_install/channel"

# Put `grove` on PATH via a stable symlink → current/bin/grove. A running process
# keeps its mapped binary, so future flips never disturb it.
#
# GROVE_LINK_DIR pins the destination and skips the search below. The search is the
# one step of this script that is NOT contained by $HOME or either grove root: its
# first candidate is /usr/local/bin, whose writability is a property of the host rather
# than of the redirected home, so on a box where the invoking user can write it a
# hermetic-looking run (the smoke test, a sandbox, a container) reaches out and
# clobbers the operator's real `grove` link with `ln -sf`. Anything that redirects
# HOME must pin this too.
grove_bin="$grove_install/current/bin/grove"
link_dir="${GROVE_LINK_DIR:-}"
linked=""
if [ -n "$link_dir" ]; then
  mkdir -p "$link_dir"
  ln -sf "$grove_bin" "$link_dir/grove"
  linked="$link_dir"
else
  for d in /usr/local/bin "$HOME/.local/bin"; do
    if [ -d "$d" ] && [ -w "$d" ]; then
      ln -sf "$grove_bin" "$d/grove"
      linked="$d"
      break
    fi
  done
  if [ -z "$linked" ]; then
    mkdir -p "$HOME/.local/bin"
    ln -sf "$grove_bin" "$HOME/.local/bin/grove"
    linked="$HOME/.local/bin"
    echo "grove: $linked is not on your PATH — add it to use \`grove\` directly"
  fi
fi

echo "grove: linked → $linked/grove"
echo "grove: done. Run 'grove on' to start the server."
