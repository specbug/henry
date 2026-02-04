# Henry - Implementation Plan

Personal home server daemon for Mac Mini M4 Pro. Security-first, minimal Rust implementation.

## Design Philosophy

- Dieter Rams' "less but better"
- Jony Ive's purposeful design
- Andy Matuschak's progressive disclosure

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                     HENRY - Mac Mini M4 Pro                        │
├─────────────────────────────────────────────────────────────────────┤
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐               │
│  │ TUI         │   │ Telegram    │   │ HTTP API    │               │
│  │ (Ratatui)   │   │ (Teloxide)  │   │ (Axum)      │               │
│  └──────┬──────┘   └──────┬──────┘   └──────┬──────┘               │
│         └─────────────────┼─────────────────┘                       │
│                    ┌──────┴──────┐                                  │
│                    │ Message Bus │                                  │
│                    │ (Tokio MPSC)│                                  │
│                    └──────┬──────┘                                  │
│  ┌──────────────┬─────────┼─────────┬──────────────┐               │
│  │              │         │         │              │               │
│  ▼              ▼         ▼         ▼              ▼               │
│ ┌────┐       ┌────┐    ┌────┐   ┌────┐        ┌────┐              │
│ │Srvr│       │Mdia│    │Clde│   │Mnt │        │Hlth│              │
│ │Mod │       │Mod │    │Mod │   │Mod │        │Mgr │              │
│ └─┬──┘       └─┬──┘    └─┬──┘   └─┬──┘        └─┬──┘              │
│   │            │         │        │             │                  │
│ Podman     Jellyfin   Anthropic  Cron        Probes               │
│            /Plex      /Claude    Backup      Alerts               │
└─────────────────────────────────────────────────────────────────────┘
```

## Tech Stack

| Component | Choice | Rationale |
|-----------|--------|-----------|
| Language | Rust | Memory safety, security, performance |
| TUI | Ratatui + Crossterm | Modern, flexible |
| Telegram | Teloxide | Bot API (simpler, audited) |
| HTTP | Axum | Tower ecosystem, WebSocket support |
| Containers | Podman | Rootless, daemon-less, secure |
| Config | TOML | Human-readable, Rust-native |
| Secrets | 1Password CLI (`op`) | Existing setup, secure |
| Database | SQLite (sqlx) | Embedded, zero-config |
| Async | Tokio | Industry standard |

## Directory Structure

```
henry/
├── Cargo.toml                    # Workspace manifest
├── config.example.toml
├── crates/
│   ├── henry-core/               # Daemon loop, CLI, signals
│   ├── henry-config/             # TOML parsing, hot reload
│   ├── henry-lock/               # Single-instance (port + PID)
│   ├── henry-secrets/            # 1Password CLI integration
│   ├── henry-state/              # SQLite state
│   ├── henry-health/             # Health probes, metrics
│   ├── henry-tui/                # Ratatui interface
│   ├── henry-telegram/           # (Phase 2) Teloxide bot
│   ├── henry-http/               # (Phase 2) Axum API
│   ├── henry-server/             # (Phase 3) Podman container mgmt
│   ├── henry-media/              # (Phase 4) Jellyfin/Plex
│   ├── henry-claude/             # (Phase 5) Anthropic API + Claude Code
│   └── henry-maint/              # (Phase 6) Updates, backup, cleanup
└── tests/

Runtime (~/.henry/):
├── config.toml
├── henry.pid
├── state/*.db
├── logs/
├── backups/
└── cache/
```

---

## Phase 1: Foundation ✅ COMPLETE

**Commit:** `c8bb5bf6a` on branch `init`

### Implemented Crates

| Crate | Purpose | Key Features |
|-------|---------|--------------|
| `henry-core` | Main binary + daemon | CLI (clap), daemon loop, signal handling |
| `henry-config` | Configuration | TOML parsing, validation, hot reload via notify |
| `henry-lock` | Instance locking | Port binding + PID file with stale detection |
| `henry-secrets` | Secret management | 1Password CLI (`op://vault/item/field` refs) |
| `henry-state` | Persistence | SQLite: module states, KV store, audit log |
| `henry-health` | Monitoring | CPU, memory, disk metrics via sysinfo |
| `henry-tui` | Terminal UI | Ratatui dashboard, modules, logs views |

### CLI Commands

```bash
henry run              # Start with TUI
henry run --headless   # Start without TUI (for services)
henry status           # Check if daemon is running
henry check            # Validate configuration
henry init             # Generate config.toml
henry info             # Show version and features
henry --config <path>  # Use specific config file
henry --verbose        # Enable debug logging
```

### Signal Handling

- `SIGTERM` / `SIGINT`: Graceful shutdown
- `SIGUSR1` / `SIGHUP`: Reload configuration

### Configuration

Secrets use 1Password references:
```toml
[telegram]
token_ref = "op://Private/Henry Telegram Bot/credential"

[claude]
api_key_ref = "op://Private/Anthropic API/credential"
```

### Tests

15 unit tests passing:
- `henry-config`: 3 tests (default, validation, serialization)
- `henry-health`: 4 tests (format_bytes, format_duration, module_health, overall_status)
- `henry-lock`: 2 tests (acquire/release, stale cleanup)
- `henry-secrets`: 3 tests (redact_reference, reference_validation, invalid_reference)
- `henry-state`: 3 tests (module_state, kv_store, audit_log)

### Binary

- Size: 4.8MB (release, LTO, stripped)
- Platform: macOS arm64

---

## Phase 2: Communication (COMPLETE)

**Completed:** 2026-02-04

### Implemented Crates

| Crate | Purpose | Key Features |
|-------|---------|--------------|
| `henry-telegram` | Telegram bot | Teloxide, allowlist auth, /status /modules /metrics commands |
| `henry-http` | HTTP API | Axum REST API, /health /status /modules /metrics /snapshot endpoints, CORS, optional token auth |

### Commands

**Telegram Bot:**
- `/start` - Start the bot
- `/help` - Show available commands
- `/status` - Show system status
- `/modules` - Show module health
- `/metrics` - Show system metrics

**HTTP API Endpoints:**
- `GET /` - API info
- `GET /health` - Health check (no auth required)
- `GET /api/status` - System status
- `GET /api/modules` - Module health
- `GET /api/metrics` - System metrics
- `GET /api/snapshot` - Full health snapshot

### Security Features

1. **Telegram allowlist auth**:
   - Check user ID first (faster)
   - Fall back to username check
   - Timing-safe string comparison via `subtle::ConstantTimeEq`

2. **HTTP API security**:
   - Bind to `0.0.0.0` with optional token for non-local requests
   - Bearer token authentication with timing-safe comparison
   - CORS enabled for local network access
   - Skip auth for localhost/192.168.x/10.x requests

3. **Daemon integration**:
   - Services start in headless mode via `tokio::spawn`
   - Shared health manager via `Arc<RwLock<HealthManager>>`
   - Graceful shutdown via `broadcast::channel`

### Tests

10 new unit tests (25 total):
- `henry-telegram`: 6 tests (auth allowlists, token verification, commands)
- `henry-http`: 4 tests (health endpoint, root endpoint, token verification, local IP detection)

---

## Phase 3: Containers

### `henry-server`
- Podman integration via `bollard` crate
- Container lifecycle: start, stop, restart, logs
- Compose file support
- Tailscale serve exposure

### Commands
- `/containers list` - Show running containers
- `/container start <name>` - Start container
- `/container stop <name>` - Stop container
- `/container logs <name>` - Get recent logs

---

## Phase 4: Media

### `henry-media`
- Jellyfin container management
- Library path configuration
- Hardware transcoding (VideoToolbox on macOS)
- Library scan triggers

---

## Phase 5: AI Integration

### `henry-claude`
- Anthropic API client
- Claude Code process spawning
- Session management
- Workspace isolation per session

### Commands
- `/claude new <workspace>` - Start new session
- `/claude list` - Show active sessions
- `/claude stop <id>` - Stop session

---

## Phase 6: Self-Maintenance

### `henry-maint`
- Cron scheduler (`tokio-cron-scheduler`)
- Automated backups
- Log rotation
- Self-update checks

---

## Security Patterns (from OpenClaw)

1. **Timing-safe comparison**: `subtle::ConstantTimeEq` for tokens
2. **File permissions**: 0o600 files, 0o700 directories
3. **SSRF prevention**: Block private IPs, link-local, metadata endpoints
4. **Safe command execution**: Args array, never shell interpolation
5. **Single-instance locking**: Port binding + PID file
6. **Allowlist auth**: Telegram user IDs/usernames, not open access

---

## Reference Files (OpenClaw)

These files have patterns worth referencing:
- `src/gateway/auth.ts` - Timing-safe auth
- `src/infra/net/ssrf.ts` - SSRF prevention
- `src/cli/gateway-cli/run-loop.ts` - SIGUSR1 restart, shutdown
- `src/infra/gateway-lock.ts` - Locking with stale detection
- `src/telegram/` - Telegram integration patterns

---

## Development Commands

```bash
# Build
cd henry
cargo build              # Debug build
cargo build --release    # Release build

# Test
cargo test               # Run all tests

# Run
./target/debug/henry run           # TUI mode
./target/debug/henry run --headless  # Headless

# Check config
./target/debug/henry --config config.example.toml check
```

---

## Networking Setup

**Static IP** (recommended):
- DHCP reservation in router, or
- Manual static IP on Mac

**Config:**
```toml
[network]
bind_address = "0.0.0.0"  # All interfaces
static_ip = "192.168.1.100"  # For docs/healthchecks

[network.tailscale]
enabled = true
hostname = "henry"
serve_https = true
```

---

## Git Info

- **Repository**: Working in `henry/` subdirectory
- **Branch**: `init`
- **Remote**: `origin` → `https://github.com/specbug/openclaw.git`
