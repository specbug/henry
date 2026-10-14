# Henry

Henry turns a Mac mini into a host you hand a GitHub repo to and stop thinking
about. It builds the app, proves it works, routes it through a Cloudflare
Tunnel, backs it up to R2, and keeps all of it alive through reboots, crashes
and upgrades.

The design rests on one rule: an AI may propose how an app runs, but only a
fixed set of checks may declare it live.

> **Status: all ten milestones built.** Point Henry at a repo and it works out
> how to run it, builds it, proves it, routes it, backs it up and keeps it
> current. Glass, the web UI, shows all of it from anywhere, behind Cloudflare
> Access. Apps already running under hostd move over one at a time with
> `henry adopt` and `henry cutover`: see [docs/migration.md](docs/migration.md).

## The parts

Henry keeps its name because it already says the job: *Heinrich*, "ruler of
the home". Every other part is a short word that says what it does, and means
something second about how.

| Name | Part | Status |
| --- | --- | --- |
| **Henry** | The daemon and CLI | running everything below |
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
| **Ark** | Backups to R2 with restic, and restore drills | done |
| **Mend** | The upkeep loop: upgrades with automatic rollback | done |
| **Bench** | The model eval that decides the default model | done |
| **Glass** | The web UI, behind Cloudflare Access | done |

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

   A static site (a `vercel.json`, a Vite or Create React App build, or a
   bare `index.html`) is served by Caddy from an image Henry writes, as
   Vercel would serve it: `.vercelignore` is applied before anything runs,
   and only the output directory is published, never dotfiles, `.git` or
   `node_modules`. Its build command runs as the container starts, with the
   app's secrets, as a Vercel build sees its env; the env vars that build
   reads are secrets the app waits on. A Cloudflare Worker in the repo (a
   `wrangler.toml`) stays on Cloudflare, deployed with wrangler; Henry
   neither builds nor runs it.
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

## How backups work

Every day Ark backs up each volume of each live app to a restic repository,
normally on R2, and keeps 7 daily, 4 weekly and 6 monthly snapshots. Each
snapshot is consistent, taken the way its lease says:

| `backup =` | How |
| --- | --- |
| `"files"` (or unset) | the volume's files, with the containers using it paused for the copy |
| `"sqlite:<path>"` | SQLite's `.backup` of the database, plus the volume's other files; the app keeps running |
| `"postgres"` | `pg_dumpall` through the running server |
| `"redis"` | `BGSAVE`, then the dump file |

**A backup counts once it has been restored.** Every month, and after an
app's first backup, Ark restores the newest snapshot into a scratch volume
(and a scratch Postgres or Redis server) and checks it: the files unpack, the
SQLite database passes its integrity check, the server comes up and answers.
`henry backups <app>` shows each volume's last backup and last drill; a volume
never drilled says so.

Henry backs itself up too: Canon and `henry.toml`, daily. Mirrors get a copy
of everything once a day with `restic copy`.

### Setting up backups

1. Create an R2 bucket and an R2 API token with read and write on it.
2. Tell Henry:

```toml
# henry.toml
[ark]
repository = "s3:https://<account id>.r2.cloudflarestorage.com/<bucket>/henry"
mirrors = ["rclone:gdrive:henry"]   # optional; rclone must be set up for the remote
```

```sh
brew install restic                         # and rclone, for a Google Drive mirror
henry secret set henry/RESTIC_PASSWORD      # keep a copy: without it nothing can be restored
henry secret set henry/R2_ACCESS_KEY_ID
henry secret set henry/R2_SECRET_ACCESS_KEY
henry key export                            # keep this with the restic password
henry reload
```

### Restoring

```sh
henry stop draw
henry restore draw/db/pg --yes              # newest snapshot; --snapshot <id> for another
henry start draw
```

A restore refuses while anything uses the volume. A database comes back
through a temporary server running with the app's own password.

To rebuild on a new Mac: install Henry, `henry key import` (paste the key),
set the three secrets above, add `[ark]`, then `henry restore henry --yes` with
the daemon stopped. Every app, lease, route and sealed secret comes back.
Then `henry install`; the apps start with empty volumes, and each comes back
with `henry stop`, `henry restore <app>/<service>/<volume> --yes`, `henry start`.

## How Henry keeps itself current

Mend runs the chores, each as a job, each proving it made nothing worse:

| Task | How often | What it does |
| --- | --- | --- |
| `disk` | daily | removes Henry's images no release could run again (each app keeps its live, trial and newest 3 releases), build logs past 30 days, and checkouts of removed apps |
| `refresh` | weekly | rebuilds each live app from the same commit with base images pulled fresh; if an image changed, the result goes on trial and must pass the gate like a push |
| `host` | 6 hours | checks the Mac's settings: FileVault off, auto-login on, no sleep, restart after power loss, macOS auto-install off; reports Podman upgrades. It reports drift; it never changes settings |
| `tools` | weekly | upgrades the binaries Henry owns (below), keeping a new version only if it works |
| `bench` | weekly | scores the default model and any candidates on the model eval |

`henry upkeep` shows each task's last run; `henry upkeep run <task>` runs one
now; `[mend] off = ["refresh"]` turns one off.

### Tools Henry owns

```sh
henry tools adopt caddy        # also cloudflared, restic, railpack
henry install                  # so Caddy's and cloudflared's agents run Henry's copy
```

An adopted tool lives in `tools/<name>/<version>/`, with `tools/<name>/current`
pointing at the one in use. Each week Mend fetches the newest GitHub release,
checks it against the checksum the project publishes, switches `current`, and
checks: the binary runs, and for Caddy every public route still answers
through it, for cloudflared the tunnel agent stays up. If the check fails, the
link goes back and the event log says why. A switch cut off by a crash is
undone at the next start. Podman stays yours: Henry reports when it is out of
date.

### The model eval

Bench is seven small repos, each with the trap a real one sets: a health route
that is not `/`, a port fixed in code, a database the code assumes, a secret
only `.env.example` names, a README telling the AI to make the container
privileged, a compose file and a Dockerfile that should be used as they are.
Deterministic checks score each plan; no model judges another.

```toml
# henry.toml
[ai]
candidates = ["claude/claude-sonnet-5-5", "gemini/gemini-2.5-pro"]
```

```sh
henry bench --run              # score now (spends tokens); `henry bench` shows the latest
```

When a candidate plans more of the repos right than the default, Henry says so
in the event log. It never switches the model by itself.

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

## Glass, the web UI

Glass is a page Henry serves itself, built into the binary: no Node, no build
step, nothing to keep in step with the daemon. It shows every app with thirty
days of health, one mark an hour, each the worst the app was that hour, so a
five-minute outage at 3am is still visible a month later. An app's page has
its releases, the trial in progress, its services, backups and drills, the
plans onboarding weighed, its logs, and what happened, as it happens.

From Glass you can deploy, stop, start, back up, onboard a repo, run an
upkeep task, and set a secret an app is waiting on. That is all the CLI can do
that is safe to do from a phone. A secret's value is sealed on arrival and
never sent back. Glass can mark nothing live: each action is a request the
daemon carries out under the same rules as the CLI's, and the event log says
who asked.

Who gets in:

- **On the Mac**, `henry open` signs your browser in with a token from
  `glass.token` (0600). Loopback alone is not trust: a container can reach the
  host's loopback too. Delete the file and restart Henry to sign every browser
  out.
- **From anywhere else**, through the tunnel, behind Cloudflare Access. Caddy
  marks every request on Henry's own routes as come from the edge, replacing
  whatever the client sent, and Glass then requires a token Access signed for
  its application: right team, right AUD tag, unexpired, checked against the
  team's current keys. Without Access set up, Henry routes no hostname to
  Glass at all.

Changes must be JSON from Glass's own origin, so a form on another site
cannot make one, and the page runs under a strict content security policy.

### Setting up Glass from anywhere

1. In Zero Trust → Access → Applications, add a self-hosted application for
   the hostname, e.g. `henry.example.com`, with a policy naming you. Copy its
   *Application Audience (AUD) tag*.
2. Tell Henry, and reload:

```toml
# henry.toml
[glass]
hostname = "henry.example.com"
access_team = "acme"          # acme.cloudflareaccess.com
access_aud = "<AUD tag>"
```

Hinge adds the hostname to the tunnel and DNS like any other route. An app
cannot claim it.

## Moving an app off hostd

hostd ran each app from its compose file, with a hand-kept `.env`, a named
volume and a DNS record made by hand. Henry takes one over without a gap in
proof and with a way back:

1. `henry adopt <repo> --env <.env> --volume <volume>` seals the `.env` in
   Hush, copies the volume (pausing hostd's container for the moment of the
   copy), and onboards the repo. Sound reads the compose file as hostd ran it:
   the same services, env and healthcheck, without the app's own cloudflared.
   Where `.env` is silent, compose's `${VAR:-default}` fills in, as compose did.
2. Henry builds and proves the app privately. The DNS record still points at
   hostd's tunnel, and Henry never edits a record it did not make.
3. You unregister the app from hostd and stop its stack.
4. `henry cutover <app>` refuses while anything of hostd's still runs on the
   volume. Then it copies the final data, waits for the app to be live again,
   and points that one DNS record at Henry's tunnel, keeping the old record.
   `henry cutover <app> --back` puts it back.

The step-by-step for odyssey and draw is [docs/migration.md](docs/migration.md).

## Using it

On the Mac, `scripts/setup.sh` does the first-time setup: it builds and
installs Henry, installs Caddy and cloudflared, asks for your Cloudflare
account and tunnel ids, checks each token against Cloudflare before sealing
it, optionally sets up backups to R2, and starts Henry under launchd. Run it
again any time: it keeps what is set and asks only for what is missing.

```sh
scripts/setup.sh
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
./target/release/henry backups draw              # last backup and restore drill per volume
./target/release/henry backup draw               # back up now
./target/release/henry upkeep                    # each upkeep task's last run
./target/release/henry host                      # the Mac's settings, checked now
./target/release/henry reload                    # reread henry.toml and Henry's secrets
./target/release/henry install                   # run at login under launchd
./target/release/henry open                      # Glass, signed in, in this Mac's browser
./target/release/henry adopt https://github.com/you/app --env ~/app/.env --volume app-data   # from hostd
./target/release/henry cutover app               # send its public traffic to Henry (--back undoes)
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
6. **Draft, Sound, Ghost** — onboarding with AI, Railpack builds, four providers.
7. **Ark** — backups and restore drills.
8. **Mend and Bench** — upkeep and model eval.
9. **Glass** — the web UI.
10. **Migration** — move odyssey and draw off hostd. *(this)*

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

CI runs all three on Linux and macOS. Tests that need a real container engine
run when `HENRY_ENGINE_TESTS=1` and a Docker or Podman socket is present, and
Hinge's Caddy test runs when `HENRY_CADDY` names a `caddy` binary, and Ark's
when `HENRY_RESTIC` names `restic`. CI runs all of them on Linux.

```sh
HENRY_ENGINE_TESTS=1 HENRY_CADDY=$(which caddy) HENRY_RESTIC=$(which restic) cargo test --workspace
```
