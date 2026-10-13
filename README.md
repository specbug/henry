# Henry

Henry turns a Mac mini into a host you hand a GitHub repo to and stop thinking
about. It builds the app, proves it works, routes it through a Cloudflare
Tunnel, backs it up to R2, and keeps all of it alive through reboots, crashes
and upgrades.

The design rests on one rule: an AI may propose how an app runs, but only a
fixed set of checks may declare it live.

> **Status: milestone 1 of 9 — foundations.** Leases, Canon, Hush and the CLI
> work today. Henry does not yet build or run anything; that arrives with Hold
> and Plumb in milestone 2. See [Build order](#build-order).

## The parts

Henry keeps its name because it already says the job: *Heinrich*, "ruler of
the home". Every other part is a short word that says what it does, and means
something second about how.

| Name | Part | Status |
| --- | --- | --- |
| **Henry** | The daemon and CLI | skeleton |
| **Canon** | SQLite desired state: apps, routes, releases, jobs, events | done |
| **Lease** | The per-app spec, `lease.toml`, and its policy checks | done |
| **Hush** | Secrets, sealed at rest with a key in the macOS Keychain | done |
| **Plumb** | The reconcile loop that closes drift | milestone 2 |
| **Hold** | The Podman VM and the runtime interface | milestone 2 |
| **Hinge** | cloudflared and Caddy: routes that swing atomically | milestone 3 |
| **Proof** | The Live gate: soak, probes, restart test, public probe | milestone 4 |
| **Draft** | Onboarding, from repo URL to Live | milestone 5 |
| **Sound** | The repo scan that uses no AI | milestone 5 |
| **Ghost** | The AI, which proposes and never decides | milestone 5 |
| **Ark** | Backups to R2 with restic, and restore drills | milestone 6 |
| **Mend** | The upkeep loop: upgrades with automatic rollback | milestone 7 |
| **Bench** | The model eval that decides the default model | milestone 7 |
| **Glass** | The web UI, behind Cloudflare Access | milestone 8 |

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

## Using it

```sh
cargo build --release
./target/release/henry check lease.toml          # policy-check a lease
./target/release/henry add lease.toml --repo https://github.com/you/draw
./target/release/henry secret set draw/SESSION_KEY   # value read from stdin
./target/release/henry apps                      # every app and its state
./target/release/henry show draw                 # routes, secrets, releases, events
./target/release/henry install                   # run at login under launchd
```

Henry keeps everything in `~/Library/Application Support/Henry`, outside
`~/Documents` so macOS privacy controls never block it. `henry where` prints
the paths; `HENRY_HOME` overrides them.

## Build order

1. **Foundations** — Canon, Lease, Hush, CLI, daemon skeleton, launchd. *(this)*
2. **Hold and Plumb** — Podman runtime and the reconcile loop.
3. **Hinge** — Caddy routes, health-gated switches, tunnel ingress and DNS.
4. **Proof** — the Live gate.
5. **Draft, Sound, Ghost** — onboarding with AI, four providers.
6. **Ark** — backups and restore drills.
7. **Mend and Bench** — upkeep and model eval.
8. **Glass** — the web UI.
9. **Migration** — move odyssey and draw off hostd.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

CI runs all three on Linux and macOS.
