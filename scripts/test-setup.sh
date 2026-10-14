#!/usr/bin/env bash
# Run scripts/setup.sh for real on a Mac, twice, with Cloudflare, GitHub, R2
# and Podman faked and everything else real: Homebrew, launchd, the Keychain.
# The second run is with Henry's agents already up, the case a plain install
# never meets. Afterwards the agents are uninstalled and the home removed.
#
#   scripts/test-setup.sh [path/to/henry]     # default: target/debug/henry
#
# CI runs it on a macOS runner. On a Mac of your own it replaces the dev.henry
# LaunchAgents for the duration, so do not run it on the Mac that hosts.
set -euo pipefail

cd "$(dirname "$0")/.."
[[ $(uname) == Darwin ]] || { echo "test-setup.sh needs macOS: it installs LaunchAgents" >&2; exit 1; }
BIN=$(cd "$(dirname "${1:-target/debug/henry}")" && pwd)/$(basename "${1:-target/debug/henry}")
[[ -x $BIN ]] || { echo "no henry at $BIN; cargo build -p henry first" >&2; exit 1; }

WORK=$(mktemp -d)
export HENRY_HOME="$WORK/home" HENRY_BIN="$BIN" FAKES="$WORK/fakes"
mkdir -p "$HENRY_HOME" "$WORK/bin"
cleanup() { "$BIN" uninstall >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT

# Real tools first, so Homebrew never runs with the fakes on its PATH.
for f in caddy cloudflared restic; do
    brew list --formula "$f" >/dev/null 2>&1 || brew install --quiet "$f"
done

# Cloudflare and GitHub: only the tokens "cf-good" and "gh-good" may do anything.
cat >"$WORK/bin/curl" <<'FAKE'
#!/bin/sh
cfg=$(cat); url=""; for a in "$@"; do url=$a; done
case "$url" in
  *api.github.com*) case "$cfg" in *"Bearer gh-good"*) printf 200;; *) printf 401;; esac; exit 0;;
esac
case "$cfg" in *"Bearer cf-good"*) ok=true;; *) ok=false;; esac
case "$url" in
  *"/zones?name="*) if $ok; then echo '{"success":true,"result":[{"id":"0123456789abcdef0123456789abcdef"}]}'; else echo '{"success":true,"result":[]}'; fi;;
  *) echo "{\"success\":$ok}";;
esac
FAKE
# R2: only the key pair r2-id / r2-secret reaches the bucket, where there is no repository yet.
cat >"$WORK/bin/restic" <<'FAKE'
#!/bin/sh
[ "$AWS_ACCESS_KEY_ID" = r2-id ] && [ "$AWS_SECRET_ACCESS_KEY" = r2-secret ] && exit 10
echo "Fatal: Access Denied." >&2; exit 1
FAKE
printf '#!/bin/sh\nexit 0\n' >"$WORK/bin/podman"
chmod +x "$WORK"/bin/*
export PATH="$WORK/bin:$PATH"

agents() { launchctl list | grep -c 'dev\.henry' || true; }
expect_agents() {
    local n
    n=$(agents)
    [[ $n == 3 ]] || { echo "FAIL: $n of 3 agents loaded after $1" >&2; launchctl list | grep dev.henry >&2; exit 1; }
    echo "ok: 3 agents loaded after $1"
}

TUNNEL=11111111-2222-3333-4444-555555555555
TT=$(printf '{"a":"acct","t":"%s","s":"c2VjcmV0"}' "$TUNNEL" | base64)

echo "== first run"
# Each answer in order: account, tunnel, zone; a bad then a good API token;
# a cut-short then a real tunnel token; private repos, a bad then a good
# GitHub token; backups, the bucket, bad then good R2 keys; the password twice.
printf '%s\n' acct "$TUNNEL" example.com \
    cf-bad y cf-good \
    ey y "sudo cloudflared service install $TT" \
    y gh-bad y gh-good \
    y "" bad-id bad-secret y r2-id r2-secret \
    pw1 pw1 | scripts/setup.sh
expect_agents "the first run"
for s in CLOUDFLARE_API_TOKEN CLOUDFLARE_TUNNEL_TOKEN GITHUB_TOKEN R2_ACCESS_KEY_ID R2_SECRET_ACCESS_KEY RESTIC_PASSWORD; do
    "$BIN" secret list henry | grep -qx "$s" || { echo "FAIL: $s was not sealed" >&2; exit 1; }
done
[[ $(cat "$HENRY_HOME/tunnel.token") == "$TT" ]] || { echo "FAIL: the tunnel token file is not the token" >&2; exit 1; }
echo "ok: every secret sealed, and the tunnel token file holds exactly the token"

echo "== second run, with Henry's agents up"
printf 'n\nn\nn\nn\n' | scripts/setup.sh
expect_agents "a second run"

echo "== henry install once more, directly"
"$BIN" install
expect_agents "a reinstall"
echo "all good"
