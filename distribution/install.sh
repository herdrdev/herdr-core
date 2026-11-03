#!/bin/sh
# Installs herdr-core (hc), or updates it:
#
#   curl -fsSL https://releases.herdr.dev/core/install.sh | sh
#
# Into ~/.local/bin/herdr-core, with `hc` beside it only if no `hc` exists
# (some window managers ship one). An hc that is already there is swapped
# the planned way (hc update --from): running terminals keep running. One
# from a package manager is left to it. HC_VERSION=0.2.0 picks a version;
# HERDR_RELEASES_URL points elsewhere (tests).
set -eu

releases=${HERDR_RELEASES_URL:-https://releases.herdr.dev}
bin=${HC_INSTALL_DIR:-$HOME/.local/bin}

say() { printf '%s\n' "$*"; }
fail() { say "hc: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "needs $1"; }
need curl
need tar

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) fail "hc runs on Linux and macOS for now" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) fail "no build for $(uname -m)" ;;
esac
target="$os-$arch"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

manifest="$releases/core/latest.json"
[ -n "${HC_VERSION:-}" ] && manifest="$releases/core/$HC_VERSION/manifest.json"
curl -fsSL "$manifest" -o "$tmp/manifest.json" || fail "cannot reach $manifest"
# Read without jq: the manifest is flat enough for sed once spaces are gone.
flat=$(tr -d '\n ' < "$tmp/manifest.json")
entry=$(printf '%s' "$flat" | sed -n "s/.*\"$target\":{\([^}]*\)}.*/\1/p")
url=$(printf '%s' "$entry" | sed -n 's/.*"url":"\([^"]*\)".*/\1/p')
want=$(printf '%s' "$entry" | sed -n 's/.*"sha256":"\([^"]*\)".*/\1/p')
version=$(printf '%s' "$flat" | sed -n 's/^{"version":"\([^"]*\)".*/\1/p')
[ -n "$url" ] && [ -n "$want" ] || fail "no build for $target in $manifest"

# Tells the release server's download counter a new install from an update;
# the file is the same either way. Only plain URLs on the release server in
# use are marked: one with a query or fragment (a mirror's signed URL) is
# fetched exactly as the manifest gives it.
src=install
if [ -x "$bin/herdr-core" ]; then src=update; fi
case $url in
  *[?#]*) download=$url ;;
  "$releases"/*) download="$url?src=$src" ;;
  *) download=$url ;;
esac
curl -fsSL "$download" -o "$tmp/hc.tar.gz" || fail "download failed: $url"
[ "$(sha256 "$tmp/hc.tar.gz")" = "$want" ] || fail "the download is damaged (checksum mismatch); try again"
tar -xzf "$tmp/hc.tar.gz" -C "$tmp"
"$tmp/herdr-core" --version >/dev/null 2>&1 || fail "the downloaded hc does not run on this machine"
# App completion must converge even before the first bundled release exists.
if [ "${HC_BUNDLE_ONLY:-0}" = 1 ] && [ ! -x "$tmp/Herdr Core.app/Contents/MacOS/herdr-core" ]; then
  exit 0
fi
mkdir -p "$bin"

bundle=0
if [ "$os" = macos ] && [ "$(id -u)" != 0 ]; then
  case "$bin" in "$HOME"/*) bundle=1 ;; esac
fi
if [ "$bundle" = 1 ] && [ -x "$tmp/Herdr Core.app/Contents/MacOS/herdr-core" ]; then
  action=installed
  [ ! -x "$bin/herdr-core" ] || action=updated
  bundled="$tmp/Herdr Core.app/Contents/MacOS/herdr-core"
  if [ "$action" = updated ]; then
    installed=$("$bin/herdr-core" --version)
    candidate=$("$bundled" --version)
    awk -v old="$installed" -v new="$candidate" 'BEGIN {
      sub(/^hc /, "", old); sub(/^hc /, "", new)
      split(old, a, /[.-]/); split(new, b, /[.-]/)
      for (i=1; i<=3; i++) { if (a[i]+0 > b[i]+0) exit 1; if (a[i]+0 < b[i]+0) exit 0 }
    }' || fail "$candidate is older than $installed; use hc update --from with --force to downgrade"
  fi
  case "$releases" in
    https://releases.herdr.dev | https://releases.herdr.dev/)
      /usr/bin/codesign --verify --strict -R '=identifier "dev.herdr.core" and anchor apple generic and certificate leaf[subject.OU] = "2W9652Q7QR"' "$tmp/Herdr Core.app" || fail "not a signed Herdr release" ;;
  esac
  "$bundled" --hc-install-bundle "$bundled" "$bin/herdr-core" || fail "installing Herdr Core.app failed"
  # A fresh install must not start a core (especially not a system/root core).
  if [ "$action" = updated ]; then
    "$bin/herdr-core" server restart >/dev/null || fail "restarting the core failed"
  fi
  say "hc $version $action as Herdr Core.app. Recreate pre-update terminals if macOS privacy access is broken; running terminals were kept."
elif [ -x "$bin/herdr-core" ]; then
  # Swapped the planned way, so its terminals keep running.
  "$bin/herdr-core" update --from "$tmp/herdr-core" >/dev/null || fail "updating $bin/herdr-core failed (see above)"
  say "hc updated to $version."
else
  cp "$tmp/herdr-core" "$bin/herdr-core.new"
  chmod 755 "$bin/herdr-core.new"
  mv -f "$bin/herdr-core.new" "$bin/herdr-core"
  say "hc $version installed into $bin/herdr-core."
fi
if ! command -v hc >/dev/null 2>&1 && [ ! -e "$bin/hc" ]; then
  ln -s herdr-core "$bin/hc"
fi

case ":$PATH:" in
  *":$bin:"*) ;;
  *) say "Add $bin to your PATH to use hc." ;;
esac
say "Agents: hc --skill. People: hc --help."
