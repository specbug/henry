# CLAUDE.md

Guidance for Claude Code working in this repository.

Henry hosts GitHub repos on one Mac mini: it builds each app, proves it works,
routes it through a Cloudflare Tunnel, backs it up to R2 and keeps it alive.
The concept doc (shared separately) is the design of record. This repo is a
rewrite; v1 lives only in git history before the rewrite commit.

## Names

Each part has a short name. Use the names in code, logs and docs; the UI uses
plain words.

| Name | Crate / module | Part |
| --- | --- | --- |
| Henry | `crates/henry` | the binary: CLI, daemon, launchd |
| Canon | `crates/canon` | SQLite desired state |
| Lease | `crates/lease` | `lease.toml` types, parser, policy |
| Hush | `crates/hush` | sealed secrets |
| Plumb | `crates/plumb` | reconcile loop (`plan.rs` is pure; `lib.rs` acts) |
| Hold | `crates/hold` | `Runtime` + `Machine` traits, `Engine` (Docker API), `fake::Fake` |
| Press | `crates/press` | git checkouts, image builds, release on push (a Canon job) |
| Hinge | `crates/hinge` | Caddy routes (admin API) + Cloudflare tunnel ingress and DNS; `henry/src/edge.rs` builds it |
| Proof | `crates/proof` | the Live gate: staged trials saved in Canon, probes, live watch |
| Sound | `crates/sound` | the AI-free repo scan, its skeleton plan, and `Plan::check` |
| Ghost | `crates/ghost` | provider adapters (`openai.rs` for Meta/Gemini/OpenAI, `claude.rs`), the tool loop (`propose.rs`) |
| Draft | `crates/draft` | onboarding rounds: a Canon job (`run`) and a tick (`pass`); `henry/src/ai.rs` picks the model |
| Ark | `crates/ark` | backups: `restic.rs` (CLI, secrets in env), `engine.rs` (helper containers, scripts), drills; `henry/src/backup.rs` builds it |
| Mend | `crates/mend` | upkeep: `disk.rs`, `host.rs`, `tools.rs` (owned binaries); `henry/src/upkeep.rs` runs each task and the tool gate |
| Bench | `crates/bench` | the model eval: `cases.rs` repos with traps, scored by `faults` |
| Glass | (milestone 9) | web UI |

The daemon (`henry/src/daemon.rs`) runs Plumb, Proof, Draft's pass, Ark's and Mend's schedules,
then Hinge on a tick, and three side tasks: the job worker (Press, Draft, Ark and Mend jobs; a
Mend job's `app` is its task), Press's poll, and the loopback HTTP endpoint (`henry/src/http.rs`).

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets     # CI denies warnings
cargo fmt --all                            # rustfmt.toml: 120 columns
cargo run -p henry -- check examples/lease.toml
HENRY_HOME=/tmp/h cargo run -p henry -- run   # a throwaway home
HENRY_ENGINE_TESTS=1 cargo test --workspace   # also run against a real Docker/Podman socket
HENRY_CADDY=/path/to/caddy cargo test -p hinge   # Hinge against a real Caddy
HENRY_ENGINE_TESTS=1 HENRY_RESTIC=/path/to/restic cargo test -p ark   # Ark: real backups, drills, restores
```

In a cloud session with no engine running, `dockerd &` usually works and makes
the engine tests real.

## Rules the design depends on

- **Ghost proposes, code decides.** AI output is only ever a proposal (a lease,
  a Dockerfile, a diagnosis). It passes `Lease::parse` and then Proof. Never
  let AI output mark anything live, open a port, or see a secret value.
- **Crash-only.** Write the step to Canon before taking it. Jobs must be safe
  to run twice: `Canon::requeue_interrupted` re-runs whatever was running when
  the process died.
- **Requests never go through Henry.** The proxy and tunnel are separate
  processes so Henry can restart or replace itself without dropping traffic.
- **Henry owns its checkouts.** Clone into `Home::checkouts()`; never touch
  the user's working copies in `~/Documents` (macOS TCC blocks background
  agents there anyway).
- **Secrets.** Values exist only as `hush::Secret`, which never prints.
  `Secret::expose` is the single way out; keep its call sites few and obvious.
  Canon stores ciphertext bound to its `scope/name` slot.
- **Only the daemon starts the Podman machine.** CLI commands connect with
  `Host::connect`, which never starts anything.
- **A failed observation is not an empty one.** If `Runtime::list` fails, a pass
  must act on nothing. Plumb retries once, then skips.
- **A stateful service runs once.** Services with volumes get one container
  (`<app>-<service>`) replaced in place; never two against one volume.
- **Git never prompts.** Every `git` call has a timeout and runs with prompts
  disabled; a GitHub token travels in `GIT_CONFIG_*` env vars, never argv.
- **A failed commit is seen, not retried.** Press records a failed release so
  polling does not rebuild a broken commit forever; a new push or
  `henry deploy` tries again.
- **Only live releases get public routes.** Hinge reads `live_release`; a
  trial is never routed.
- **DNS: touch only what Henry made.** Records Henry creates carry the comment
  `managed by henry`; anything else is reported, never edited or deleted.
- **Only Proof marks a release Live.** Nothing else calls
  `set_release_state(.., Live, ..)`; no CLI command, no AI output.
- **Promotion never recreates the proven container.** Aliases are out of the
  spec hash; Plumb moves them in place with `Runtime::set_aliases`.
- **A trial survives a restart, and waits on you.** Proof saves its stage in
  the `proofs` table (in memory: only the live watch's failure counts). A
  trial whose app needs a secret is not timed out.
- **Proof writes Degraded; Plumb does not overwrite it.** Down (containers not
  running) is Plumb's word and outranks Degraded.
- **Sound runs nothing from the repo** and keeps env var names, never secret
  values. Ghost sees only files Sound walked, minus what `sound::readable`
  rejects (`.env`, keys, credentials).
- **Plans are checked where they are made.** `sound::Plan::check` runs inside
  Ghost's loop and again in Draft. Files go only under `.henry/`: overlays,
  stored in Canon and written into each checkout by Press.
- **Deterministic first.** Draft tries the repo's lease, then Sound's skeleton,
  then Ghost, one round per plan. A transient provider error spends no round.
- **Generated secrets.** A random one is made once and kept (a database was
  initialised with it); a templated one is remade from its parts each round.
- **An image tag names its recipe.** `henry/<app>-<svc>:<sha12>-<hash>` hashes
  the build recipe and overlays, so one commit built two ways is two images.
- **A backup counts once restored.** Ark drills each target into a scratch
  volume (and server) after its first backup and monthly; `henry backups`
  says "not yet proven restorable" until one passes.
- **Ark never overwrites a volume in use.** `Ark::restore` refuses while any
  container mounts it. Scratch names are `henry-drill-<app>-<service>`.
- **Backup failures are recorded, not retried hot.** A target whose last
  attempt failed waits `retry` (1h) before the schedule queues it again.
- **Nothing stays paused.** Files backups pause a volume's users and always
  unpause; the job worker unpauses any managed container left paused at start.
- **Henry's key is not in its backups.** On the Mac it lives in the Keychain;
  `henry key export` is how it is kept, `henry key import` how it returns.
- **Upkeep proves itself or undoes itself.** A refreshed image goes on trial
  through Proof; a tool switch is kept only if `HenryGate` passes, else the
  `current` link goes back. Canon records `pending` before a switch, and
  `Tools::recover` undoes an unproven one at start.
- **Mend reports the Mac, it does not change it.** Host checks read settings;
  anything needing sudo is yours.
- **Bench never lets a model judge.** Scores come from `bench::faults` over a
  checked lease; Henry recommends a model, it never switches by itself.
- **Leases refuse, never ignore.** Every struct in `lease/src/raw.rs` has
  `deny_unknown_fields`. New capabilities get a field and a check, with a
  stable violation `code` that Ghost can act on.

## Conventions

- Canon migrations in `canon/src/schema.rs` are append-only. Never edit a
  shipped entry.
- Every crate keeps its tests next to it (`src/tests.rs` or inline). Prefer
  tests that state a behaviour (`a_value_moved_to_another_slot_will_not_open`).
- Error types: `thiserror` in libraries, `anyhow` in the binary.
- Timestamps are Unix milliseconds (`canon::now_ms`); `henry::time::utc` prints them.

## Host facts worth knowing (from hostd)

- Only one process may start the Podman VM: two concurrent
  `podman machine start` calls SIGTERM each other's gvproxy.
- A failed `podman ps` does not mean "nothing is running". Retry before
  believing it.
- A build that starts milliseconds after a checkout can see stale files in the
  VM; one retry is warranted before calling a commit broken.
- FileVault is off and auto-login is on, so LaunchAgents start after a reboot
  without anyone at the machine.
