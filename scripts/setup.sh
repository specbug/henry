#!/usr/bin/env bash
# Set Henry up on this Mac: build and install it, write henry.toml, seal its
# secrets, and start it under launchd. Run it from anywhere in the repo:
#
#   scripts/setup.sh
#
# Safe to run again: for each secret already set it asks whether to replace
# it, so a second run is how you fix a value or add backups later. Every
# token and key is checked against the service it is for before it is kept,
# and none ever appears in argv or on screen.
set -euo pipefail

cd "$(dirname "$0")/.."

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
note() { printf '  %s\n' "$*"; }
die() { printf '\n\033[31m%s\033[0m\n' "$*" >&2; exit 1; }
ask() { local a; read -r -p "  $1${2:+ [$2]}: " a; printf '%s' "${a:-${2:-}}"; }
yes_no() { local a; read -r -p "  $1 [y/N] " a; [[ $a == [yY]* ]]; }
hidden() { local v; read -r -s -p "  $1 (hidden as you paste): " v; echo >&2; printf '%s' "$v"; }
steps() { local i=1 line; for line in "$@"; do printf '    %d. %s\n' "$i" "$line"; i=$((i + 1)); done; }

# --- what this Mac needs -------------------------------------------------------

[[ $(uname) == Darwin ]] || die "This sets Henry up on macOS."
for tool in brew cargo git podman curl; do
    command -v "$tool" >/dev/null || die "$tool is not installed."
done

bold "Building Henry"
if [[ -n ${HENRY_BIN:-} ]]; then
    # A binary already built, e.g. by CI or by scripts/test-setup.sh.
    H=$HENRY_BIN
else
    cargo install --path crates/henry --locked --quiet
    H=$(command -v henry || true)
    [[ -n $H ]] || H="$HOME/.cargo/bin/henry"
fi
[[ -x $H ]] || die "henry was built but is not at $H; is ~/.cargo/bin on your PATH?"
note "$("$H" --version) at $H"

bold "Caddy and cloudflared"
for formula in caddy cloudflared; do
    brew list --formula "$formula" >/dev/null 2>&1 || brew install --quiet "$formula"
    note "$formula: $(brew list --versions "$formula")"
done

# Keys pressed while it built would answer the first question; drop them.
if [[ -t 0 ]]; then
    while read -r -t 1 -n 1000 _; do :; done
fi

# --- henry.toml ---------------------------------------------------------------

CONF=$("$H" where | sed -n 's/^config *//p')
HOME_DIR=$("$H" where | sed -n 's/^home *//p')
mkdir -p "$(dirname "$CONF")"
touch "$CONF"
has() { grep -q "^\[$1\]" "$CONF"; }
value() { sed -n "s/^$1 *= *\"\(.*\)\"/\1/p" "$CONF" | head -1; }

bold "Configuration: $CONF"
if has cloudflare; then
    note "[cloudflare] is set; keeping it"
else
    note "A tunnel for Henry, if you have not made one:"
    steps "dash.cloudflare.com → Zero Trust → Networks → Tunnels → Create a tunnel → Cloudflared." \
        "Name it henry and save. Copy the token in the install command (the string after --token)." \
        "Skip installing a connector and skip adding routes: Henry does both."
    note "Account id: on the right of any domain's Overview page. Tunnel id: in the tunnels list."
    account=$(ask "Account id")
    tunnel=$(ask "Tunnel id")
    zone=$(ask "Your domain on Cloudflare, e.g. example.com")
    [[ -n $account && -n $tunnel && -n $zone ]] || die "All three are needed."
    printf '\n[cloudflare]\naccount = "%s"\ntunnel = "%s"\nzones = ["%s"]\n' "$account" "$tunnel" "$zone" >>"$CONF"
fi
if ! has press; then
    # Some apps (draw's web bundle) take longer than the 30-minute default.
    printf '\n[press]\nbuild_timeout_minutes = 45\n' >>"$CONF"
fi
ACCOUNT=$(value account)
TUNNEL=$(value tunnel)
ZONE=$(sed -n 's/^zones *= *\["\([^"]*\)".*/\1/p' "$CONF" | head -1)

# --- secrets ------------------------------------------------------------------

is_set() { "$H" secret list henry 2>/dev/null | grep -qx "$1"; }

# Whether to (re)enter a secret: always when unset, on request when set.
wanted() { is_set "$1" || return 0; yes_no "henry/$1 is already set. Replace it?"; }

# Read one token or key. Paste it at the hidden prompt, or copy it and just
# press Enter to take it from the clipboard (which is then cleared). Tokens
# hold no spaces, so stray whitespace and newlines from a paste are dropped,
# and the length is shown so a paste that landed half-way is easy to spot.
take() {
    local v from=pasted
    read -r -s -p "  $1 (paste it, or copy it and press Enter): " v
    echo >&2
    if [[ -z $v ]]; then
        v=$(pbpaste 2>/dev/null || true)
        from="from the clipboard"
        [[ -n $v ]] && pbcopy </dev/null
    fi
    v=$(printf '%s' "$v" | tr -d '[:space:]')
    note "(${#v} characters, $from)" >&2
    printf '%s' "$v"
}
seal() { printf '%s' "$2" | "$H" secret set "henry/$1" >/dev/null && note "sealed henry/$1"; }

# Cloudflare's API with the token from a pipe, so it never shows in `ps`.
cf() { printf 'header = "Authorization: Bearer %s"\n' "$T" | curl -sS -K - "https://api.cloudflare.com/client/v4$1"; }
ok() { grep -q '"success": *true'; }

bold "Cloudflare API token"
if ! wanted CLOUDFLARE_API_TOKEN; then
    note "keeping it"
else
    note "Create one at dash.cloudflare.com/profile/api-tokens → Create Token → Custom token:"
    steps "Policy on the entire account: Cloudflare Tunnel (listed as Argo Tunnel) → Edit." \
        "Add a policy on the zone $ZONE: DNS → Edit, and Zone → Read." \
        "Create, and copy the token: it is shown once."
    note "Each permission is checked against Cloudflare before the token is kept."
    while :; do
        T=$(take "API token")
        fails=""
        cf "/accounts/$ACCOUNT/cfd_tunnel/$TUNNEL/configurations" | ok || fails+="Cloudflare Tunnel Edit (or the account/tunnel id); "
        zone_id=$(cf "/zones?name=$ZONE" | grep -o '"id": *"[a-f0-9]\{32\}"' | head -1 | grep -o '[a-f0-9]\{32\}' || true)
        if [[ -z $zone_id ]]; then
            fails+="Zone Read on $ZONE; "
        else
            cf "/zones/$zone_id/dns_records?per_page=1" | ok || fails+="DNS Edit on $ZONE; "
        fi
        if [[ -z $fails ]]; then
            seal CLOUDFLARE_API_TOKEN "$T"
            break
        fi
        note "That token cannot do: ${fails%; }"
        yes_no "Try another token?" || die "Stopped; nothing was kept."
    done
    unset T
fi

bold "Tunnel token"
# The token is base64 JSON naming its tunnel; why it is not a usable one, if not.
tunnel_problem() {
    local t=$1 json
    while ((${#t} % 4)); do t+="="; done
    json=$(printf '%s' "$t" | base64 --decode 2>/dev/null || true)
    if [[ $1 != eyJ* ]]; then
        echo "it is not a tunnel token, which is the long string starting with eyJ"
    elif [[ $json != *'"t"'* ]]; then
        echo "it does not read as one; it may have been cut short"
    elif [[ $json != *"$TUNNEL"* ]]; then
        echo "it is for a different tunnel than $TUNNEL"
    fi
}
stored=$(cat "$HOME_DIR/tunnel.token" 2>/dev/null || true)
replace_tunnel=true
if is_set CLOUDFLARE_TUNNEL_TOKEN; then
    why=$(tunnel_problem "$stored")
    if [[ -n $why && -n $stored ]]; then
        note "The tunnel token Henry has is broken ($why): enter it again."
    elif ! yes_no "henry/CLOUDFLARE_TUNNEL_TOKEN is already set and reads as valid. Replace it?"; then
        replace_tunnel=false
    fi
fi
unset stored
if ! $replace_tunnel; then
    note "keeping it"
else
    note "The token from when you made the tunnel. To see it again: Zero Trust → Networks →"
    note "Tunnels → henry → Configure; it is the long string after --token in the install command."
    while :; do
        raw=$(take "Tunnel token")
        # The dashboard shows a whole install command; the token is its eyJ… word.
        TT=$(printf '%s' "$raw" | grep -o 'eyJ[A-Za-z0-9+/=_-]*' | head -1 || true)
        why=$(tunnel_problem "${TT:-$raw}")
        [[ -z $why ]] && break
        note "Not kept: $why."
        yes_no "Try again?" || die "Stopped; no tunnel token was kept."
    done
    seal CLOUDFLARE_TUNNEL_TOKEN "$TT"
    unset TT raw why
fi

bold "GitHub"
if is_set GITHUB_TOKEN && ! yes_no "henry/GITHUB_TOKEN is already set. Replace it?"; then
    note "keeping it"
elif is_set GITHUB_TOKEN || yes_no "Are any of your repos private (Henry then needs a read token)?"; then
    note "Create a fine-grained token that can only read those repos:"
    steps "github.com/settings/personal-access-tokens/new; name it henry; pick an expiry." \
        "Repository access: Only select repositories → the ones Henry will host." \
        "Repository permissions → Contents: Read-only. Leave everything else at No access." \
        "Generate token, and copy it (github_pat_…): it is shown once."
    while :; do
        G=$(take "GitHub token")
        # Any valid token may read the rate limit; a bad one gets 401.
        code=$(printf 'header = "Authorization: Bearer %s"\n' "$G" |
            curl -sS -o /dev/null -w '%{http_code}' -K - https://api.github.com/rate_limit || true)
        if [[ $code == 200 ]]; then
            seal GITHUB_TOKEN "$G"
            break
        fi
        note "GitHub did not accept that token (HTTP $code)."
        yes_no "Try another token?" || die "Stopped; no GitHub token was kept."
    done
    unset G
else
    note "public repos need no token"
fi

# --- backups ------------------------------------------------------------------

bold "Backups to R2"
if has ark; then
    note "[ark] is set; keeping it"
elif yes_no "Set up backups now? (You can run this script again later to add them.)"; then
    steps "dash.cloudflare.com → R2 Object Storage → Create bucket (any name, e.g. henry-backups)." \
        "Back on R2 Object Storage: API (on the right) → Manage API tokens → Create Account API token." \
        "Permissions: Object Read & Write. Specify bucket(s): Apply to specific buckets only → your bucket." \
        "Create API Token. The next page shows an Access Key ID and a Secret Access Key, once: copy both."
    note "These keys come only from that R2 page: your Cloudflare API token is not them, and"
    note "adding R2 permissions to it does not make them."
    bucket=$(ask "Bucket name, exactly as you created it" "henry-backups")
    printf '\n[ark]\nrepository = "s3:https://%s.r2.cloudflarestorage.com/%s/henry"\n' "$ACCOUNT" "$bucket" >>"$CONF"
    brew list --formula restic >/dev/null 2>&1 || brew install --quiet restic
    note "restic: $(brew list --versions restic)"
fi
r2_wanted=false
if has ark; then
    if ! is_set R2_ACCESS_KEY_ID || ! is_set R2_SECRET_ACCESS_KEY; then
        r2_wanted=true
    elif yes_no "The R2 keys are already set. Replace them?"; then
        r2_wanted=true
    fi
fi
if $r2_wanted; then
    REPO=$(value repository)
    command -v restic >/dev/null || brew install --quiet restic
    note "The R2 keys, from the page that appeared when you created the R2 token:"
    while :; do
        kid=$(take "R2 Access Key ID")
        ksec=$(take "R2 Secret Access Key")
        # Ask the bucket for the repository's config with these keys, through
        # the environment. restic exits 10 when the repository is not there
        # yet and 12 when it is but the password differs: either way the keys
        # reached the bucket. Anything else means they did not.
        err=$(AWS_ACCESS_KEY_ID=$kid AWS_SECRET_ACCESS_KEY=$ksec RESTIC_PASSWORD=checking-keys-only \
            RESTIC_REPOSITORY=$REPO restic cat config 2>&1 >/dev/null) && code=0 || code=$?
        if [[ $code == 0 || $code == 10 || $code == 12 ]]; then
            seal R2_ACCESS_KEY_ID "$kid"
            seal R2_SECRET_ACCESS_KEY "$ksec"
            break
        fi
        note "R2 refused them: $(printf '%s\n' "$err" | grep -v '^$' | tail -1)"
        note "Check the keys, and that the token covers the bucket in $REPO."
        yes_no "Try again?" || die "Stopped; the R2 keys were not kept. Run this script again to finish."
    done
    unset kid ksec err
fi
if has ark; then
    if is_set RESTIC_PASSWORD; then
        # Backups may already be locked with it; a new one could not open them.
        note "The backup password is set and stays as it is: backups made with it need it."
    else
        note "Choose a password for the backups and save it in your password manager:"
        note "without it, nothing can be restored."
        while :; do
            p1=$(hidden "Backup password")
            p2=$(hidden "Again")
            [[ -n $p1 && $p1 == "$p2" ]] && break
            note "They differ, or are empty; again."
        done
        seal RESTIC_PASSWORD "$p1"
        unset p1 p2
    fi
fi

# --- start --------------------------------------------------------------------

bold "Starting Henry"
"$H" install
sleep 3
running=$(launchctl list | grep -c 'dev\.henry' || true)
note "$running of 3 agents loaded (Henry, Caddy, cloudflared)"
((running == 3)) || note "Something did not start; \`launchctl list | grep dev.henry\` and \`henry events\` say why."
"$H" events -n 3 | sed 's/^/  /'

bold "Done"
if has ark; then
    note "Keep Henry's master key with your backup password; run this once and save what it prints:"
    note "  henry key export"
fi
note "Next, bring each app over, e.g.:"
note "  henry adopt https://github.com/you/app --hostname app.$ZONE --env ~/Documents/app/.env --volume app-data"
note "  henry show app      # until it says live; Henry then makes its DNS record"
note "Glass, the web UI, on this Mac: henry open"
