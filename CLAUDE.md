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
| Hinge | (milestone 4) | cloudflared + Caddy |
| Proof | (milestone 5) | the Live gate |
| Draft / Sound / Ghost | (milestone 6) | onboarding / AI-free scan / the AI |
| Ark | (milestone 7) | backups |
| Mend / Bench | (milestone 8) | upkeep / model eval |
| Glass | (milestone 9) | web UI |

The daemon (`henry/src/daemon.rs`) runs Plumb on a tick and three side tasks:
the job worker, Press's poll, and the loopback HTTP endpoint (`henry/src/http.rs`).

## Commands

```sh
cargo test --workspace
cargo clippy --workspace --all-targets     # CI denies warnings
cargo fmt --all                            # rustfmt.toml: 120 columns
cargo run -p henry -- check examples/lease.toml
HENRY_HOME=/tmp/h cargo run -p henry -- run   # a throwaway home
HENRY_ENGINE_TESTS=1 cargo test --workspace   # also run against a real Docker/Podman socket
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
