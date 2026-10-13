# Henry

Henry turns a Mac mini into a host you hand a GitHub repo to and stop thinking
about. It builds the app, proves it works, routes it through a Cloudflare
Tunnel, backs it up to R2, and keeps all of it alive through reboots, crashes
and upgrades.

The design rests on one rule: an AI may propose how an app runs, but only a
fixed set of checks may declare it live.

> **Status: milestone 6 of 10.** Point Henry at a repo and it works out how to
> run it, builds it, proves it and routes it; every push after that is built,
> soaked and goes live only if it holds. Backups are next. See
> [Build order](#build-order).

## The parts

Henry keeps its name because it already says the job: *Heinrich*, "ruler of
the home". Every other part is a short word that says what it does, and means
something second about how.

| Name | Part | Status |
| --- | --- | --- |
| **Henry** | The daemon and CLI | running Plumb, Press, Proof, Hinge and Draft |
| **Canon** | SQLite desired state: apps, routes, releases, jobs, events | done |
| **Lease** | The per-app spec, `lease.toml`, and its policy checks | done |
| **Hush** | Secrets, sealed at rest with a key in the macOS Keychain | done |
| **Plumb** | The reconcile loop that closes drift | done |
| **Hold** | The Podman VM and the runtime interface | done |
| **Press** | Fetch, build and release on every push | done |
| **Hinge** | cloudflared and Caddy: routes that swing atomically | done |
| **Proof** | The Live gate: soak, probes, restart test, public probe | done |
| **Draft** | Onboarding, from repo URL to Live, round by round | done |
| **Sound** | The repo scan that uses no AI | done |
| **Ghost** | The AI, which proposes and never decides | done |
| **Ark** | Backups to R2 with restic, and restore drills | milestone 7 |
| **Mend** | The upkeep loop: upgrades with automatic rollback | milestone 8 |
| **Bench** | The model eval that decides the default model | milestone 8 |
| **Glass** | The web UI, behind Cloudflare Access | milestone 9 |

## A lease

An app leases room on the Mac. The lease says what it may do and the rent it
owes: a health check for every service. It cannot express privileged
containers, host networking or host paths, and unknown fields are refused.

```toml
name = "draw"

[[service]]
name = "api"
port = 8100
build = { dockerfile = "Dockerfile" }
health = { http = "/health" }
public = { hostname = "draw.example.com" }
secrets = ["SESSION_KEY"]
```

[`examples/lease.toml`](examples/lease.toml) shows every field.

## How a repo becomes an app

```sh
henry onboard https://github.com/you/draw --hostname draw.example.com
henry show draw      # where it stands
henry plans draw     # every plan it weighed, in full
```

Draft works in rounds. Each round picks the most deterministic plan not yet
tried, checks it, builds it and puts it on trial:

1. **The repo's own `lease.toml`**, used as is. If it fails, you fix the repo.
2. **Sound's draft.** Sound scans the repo without AI: Dockerfiles, a compose
   file, language manifests, `.env.example`, the env var names the code reads,
   and commands from CI and Procfiles. From a compose file or a Dockerfile it
   drafts the services; from a bare manifest (`package.json`, `pyproject.toml`,
   `go.mod`, `Cargo.toml`, …) it asks Railpack to build. An app that uses
   Postgres or Redis gets its own, with a generated password and URL. Sound
   says where it guessed.
3. **Ghost's plan.** When Sound's draft fails, or there is nothing to draft
   from, Ghost reads the repo (list, read, search; never `.env` files or keys)
   and proposes a lease, and a Dockerfile under `.henry/` if one is needed.
   Each proposal is checked the moment it is made; a refusal goes straight
   back to Ghost. A build log or a failed trial goes back too, and Ghost
   revises.

A plan goes live only through the same gate as every push. Onboarding stops at
five rounds or two million model tokens, whichever comes first, and says why.
Ghost never sees a secret's value: it names secrets, and you set them
(`henry secret set draw/STRIPE_KEY`) or Henry generates them. While a secret is
missing, the trial waits for you instead of failing.

Files Ghost writes live in Canon and are laid over each checkout under
`.henry/`; Henry never commits to your repo.

### AI providers

Set a key for any of them; Henry uses the first it finds, Meta first, or the
model you name.

```sh
henry secret set henry/META_API_KEY        # or GEMINI_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY
```

```toml
# henry.toml
[ai]
default_model = "claude/claude-sonnet-5-5"   # provider/model; optional
max_rounds = 5
max_tokens = 2000000                          # per onboarding
```

Meta's API, Gemini and OpenAI are reached through their OpenAI-compatible
endpoints; Claude through its own API. `[ai.endpoints]` overrides an endpoint.
With no key at all, onboarding gets as far as the repo and Sound can take it.

### Railpack

Repos without a Dockerfile are built by [Railpack](https://railpack.com), which
needs BuildKit. Install Railpack (`brew install railpack`, or set
`[press] railpack`); Henry runs BuildKit itself as the `henry-buildkit`
container and gives Railpack a `docker` that is Podman. Without Railpack, Ghost
writes a Dockerfile instead.

## How Plumb keeps apps running

Every 15 seconds Plumb compares what Canon says should run with what the
engine has, and closes the gap.

- **Stateless services** get one container per release (`draw-web-r7`), so a
  new release can run beside the live one.
- **A service with volumes** gets exactly one container (`draw-db`), replaced
  in place. Two copies never share a volume.
- **Dependencies start first.** Nothing starts until what it depends on runs.
- **A missing secret freezes the app.** Nothing new starts, and nothing running
  is touched, until you set it.
- **A failed listing is not an empty one.** If the engine cannot list its
  containers, Plumb changes nothing that pass.
- **Failures back off.** A container that will not start is retried after 15
  seconds, doubling to 10 minutes, and logged once per streak.

Published services listen on `127.0.0.1:20000-29999`, for Hinge to route to.

## How a push becomes a release

1. **Noticed.** GitHub's push webhook queues a deploy at once. A poll of every
   auto-deploying branch (`git ls-remote`, every 60 seconds) catches anything
   the webhook missed.
2. **Fetched.** The exact commit lands in Henry's own checkout, cleaned of
   everything else. Your working copies are never touched.
3. **Leased.** A `lease.toml` committed in the repo wins over the stored one,
   after the same policy checks. A bad one fails the release, not the app.
4. **Built.** Each Dockerfile service is built with `podman build` (Mac) or
   `docker build`, tagged by commit so the same commit never builds twice. A
   failed build is retried once, then the release fails with the log's tail.
5. **On trial.** The release replaces any older trial, and Plumb runs it beside
   the live release. Only Proof can make it live.
6. **Proven.** See below. A proven release goes live, Hinge swings its routes
   to it, and Plumb retires the old one.

A commit that failed is not retried until a new push or `henry deploy`.

To accept webhooks, set a secret and give GitHub the same one:

```sh
henry secret set henry/GITHUB_WEBHOOK_SECRET   # then point GitHub at /hooks/github
henry secret set henry/GITHUB_TOKEN            # for private repos; never on argv
```

## How a release goes live

A release is live when it has endured, not when it says so. Proof takes it
through three stages, saving its progress in Canon after every pass, so a
restart of Henry or of the Mac resumes the trial instead of starting over:

1. **Up.** Every container of the release is running, within 10 minutes.
2. **Soak.** For 10 minutes nothing restarts, gets killed for memory, or logs a
   crash (a panic, a traceback, a segfault and the like), and then each
   service's health check passes 30 times in a row. A soak that has not held
   after 30 minutes fails, naming the last thing that went wrong.
3. **Restart test.** The release's own containers are stopped and started, and
   must pass their checks again. A database the live release shares is left
   alone; on an app's first release, it is restarted too.

A health check is the lease's `health`: an HTTP path that answers below 400, a
TCP connect, a Postgres SSL handshake, or a Redis `PING`.

A release that fails leaves the live one serving. An app with no live release
becomes *Stuck*, with the reason. After going live, Proof keeps checking every
service, and public ones through Caddy too. Three failures in a row make the
app *Degraded*; six get the container restarted, at most once every five
minutes. All of it is tunable:

```toml
# henry.toml
[proof]
soak_minutes = 10
streak = 30
soak_limit_minutes = 30
degraded_after = 3
restart_after = 6
signatures = ["level=fatal"]   # more log lines that mean a crash
```

## How a request reaches an app

```
visitor → Cloudflare → tunnel → cloudflared → Caddy (127.0.0.1:8080) → container (127.0.0.1:20000+)
```

Hinge keeps both ends in line with Canon on every tick:

- **Caddy** gets its whole route table through its admin API, only when it
  changed. Caddy runs under its own LaunchAgent with `--resume`, so routes
  survive a restart of Caddy, of Henry or of the Mac. Unknown hosts get a 404.
- **Only live releases are routed.** A release on trial has no public route.
- **A route swings only when the new upstream answers.** Until then it stays on
  the old release.
- **The tunnel** sends every public hostname to Caddy, then a 404 catch-all.
- **DNS**: Henry creates a proxied CNAME per hostname, tagged
  `managed by henry`. It only ever edits or deletes records carrying that tag,
  so a record you made by hand is reported, never overwritten.

Henry is not on the request path. Stop it and traffic keeps flowing.

### Setting up the tunnel

1. In the Cloudflare dashboard, create a tunnel (Zero Trust → Networks →
   Tunnels → cloudflared). Note its id and copy its token.
2. Create an API token with *Cloudflare Tunnel: Edit* on the account and
   *DNS: Edit* on your zone.
3. Tell Henry:

```toml
# henry.toml
[cloudflare]
account = "<account id>"
tunnel = "<tunnel id>"
zones = ["example.com"]

[hinge]
hooks_host = "hooks.example.com"   # GitHub's webhook: https://hooks.example.com/hooks/github
```

```sh
henry secret set henry/CLOUDFLARE_API_TOKEN
henry secret set henry/CLOUDFLARE_TUNNEL_TOKEN   # cloudflared reads it from a 0600 file
henry install                                    # Henry, Caddy and cloudflared agents
henry routes                                     # what is routed where
```

## Using it

```sh
cargo build --release
./target/release/henry onboard https://github.com/you/draw --hostname draw.example.com
./target/release/henry plans draw                # the plans onboarding weighed
./target/release/henry check lease.toml          # policy-check a lease
./target/release/henry add lease.toml --repo https://github.com/you/draw   # or bring your own lease
./target/release/henry secret set draw/SESSION_KEY   # value read from stdin
./target/release/henry deploy draw               # build and try the branch now
./target/release/henry trial draw                # try a release made of existing images, through Proof
./target/release/henry ps                        # what the engine runs, per app
./target/release/henry apps                      # every app and its state
./target/release/henry show draw                 # routes, secrets, releases, events
./target/release/henry routes                    # public hostnames and where they go
./target/release/henry reload                    # reread henry.toml and Henry's secrets
./target/release/henry install                   # run at login under launchd
```

Henry keeps everything in `~/Library/Application Support/Henry`, outside
`~/Documents` so macOS privacy controls never block it. `henry where` prints
the paths; `HENRY_HOME` overrides them.

## Build order

1. **Foundations** — Canon, Lease, Hush, CLI, daemon skeleton, launchd.
2. **Hold and Plumb** — Podman runtime and the reconcile loop.
3. **Press** — fetch, build and release on push.
4. **Hinge** — Caddy routes, health-gated switches, tunnel ingress and DNS.
5. **Proof** — the Live gate.
6. **Draft, Sound, Ghost** — onboarding with AI, Railpack builds, four providers. *(this)*
7. **Ark** — backups and restore drills.
8. **Mend and Bench** — upkeep and model eval.
9. **Glass** — the web UI.
10. **Migration** — move odyssey and draw off hostd.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

CI runs all three on Linux and macOS. Tests that need a real container engine
run when `HENRY_ENGINE_TESTS=1` and a Docker or Podman socket is present, and
Hinge's Caddy test runs when `HENRY_CADDY` names a `caddy` binary. CI runs
both on Linux.

```sh
HENRY_ENGINE_TESTS=1 HENRY_CADDY=$(which caddy) cargo test --workspace
```
