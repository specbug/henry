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

**Commit:** `177c1034f` on branch `init`
**Completed:** 2026-02-04

### Implemented Crates

| Crate | Purpose | Key Features |
|-------|---------|--------------|
| `henry-telegram` | Telegram bot | Teloxide, allowlist auth, /status /modules /metrics commands |
| `henry-http` | HTTP API | Axum REST API, /health /status /modules /metrics /snapshot endpoints, CORS, optional token auth |

### Key Files

```
crates/henry-telegram/
├── Cargo.toml
└── src/
    ├── lib.rs          # Bot setup, command handlers, run_bot()
    └── auth.rs         # is_authorized(), timing-safe comparison

crates/henry-http/
├── Cargo.toml
└── src/
    ├── lib.rs          # Router, handlers, run_server()
    └── auth.rs         # auth_middleware(), Bearer token validation

crates/henry-core/src/
└── daemon.rs           # start_services() spawns Telegram + HTTP
```

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

### Code Patterns

> **Note:** These signatures were updated in Phase 3 to include `containers` parameter. See Phase 3 for current signatures.

**Service spawning pattern** (in `daemon.rs`):
```rust
// Shared state
let health: Arc<RwLock<HealthManager>> = ...;
let (shutdown_tx, _) = broadcast::channel(1);

// Spawn service with shutdown receiver
let shutdown_rx = shutdown_tx.subscribe();
tokio::spawn(async move {
    henry_http::run_server(..., shutdown_rx).await
});

// On shutdown signal:
let _ = shutdown_tx.send(());
```

### Tests

10 new unit tests (25 total):
- `henry-telegram`: 6 tests (auth allowlists, token verification, commands)
- `henry-http`: 4 tests (health endpoint, root endpoint, token verification, local IP detection)

### Binary

- Size: 9.0MB (release, LTO, stripped) - up from 4.8MB due to Telegram/HTTP deps

---

## Phase 3: Containers (COMPLETE)

**Commit:** `283ef05b5` on branch `init`
**Completed:** 2026-02-04

### Implemented Crate: `henry-server`

**Purpose:** Podman/Docker container management via bollard

### Directory Structure

```
crates/henry-server/
├── Cargo.toml
└── src/
    ├── lib.rs          # ContainerManager, ServerModule
    ├── podman.rs       # Podman/Colima socket auto-detection
    └── types.rs        # ContainerInfo, ContainerStatus, ContainerDetail
```

### Key Features

1. **Container lifecycle**: start, stop, restart, logs, inspect
2. **Smart socket detection**: Supports Podman, Docker, and Colima
   - `$XDG_RUNTIME_DIR/podman/podman.sock`
   - `/run/user/$UID/podman/podman.sock`
   - `~/.local/share/containers/podman/machine/podman.sock`
   - `/var/run/docker.sock`
   - `~/.colima/default/docker.sock`
3. **Container allowlist**: Restrict which containers can be managed
4. **Secret filtering**: Hides sensitive env vars in inspect output
5. **Health integration**: Updates module health based on runtime availability

### Config

```toml
[containers]
enabled = true
use_podman = true
socket_path = ""  # Auto-detect if empty
allowed_containers = []  # Empty = all allowed
```

### Telegram Commands

- `/containers` - List all containers with status
- `/container start <name>` - Start a container
- `/container stop <name>` - Stop a container
- `/container restart <name>` - Restart a container
- `/container logs <name> [tail]` - Get logs (default: 50 lines)

### HTTP API Endpoints

- `GET /api/containers` - List all containers
- `GET /api/containers/{name}` - Inspect container details
- `POST /api/containers/{name}/start` - Start container
- `POST /api/containers/{name}/stop` - Stop container
- `POST /api/containers/{name}/restart` - Restart container
- `GET /api/containers/{name}/logs?tail=50` - Get container logs

### Tests

10 new unit tests (35 total):
- `henry-server`: 10 tests (container status parsing, allowlist filtering, secret detection, socket detection)

### Code Patterns (Updated)

**Service signatures now include containers** (in `daemon.rs`):
```rust
// Shared container manager
let containers: Option<Arc<RwLock<ContainerManager>>> = if config.containers.enabled {
    ContainerManager::new(config.containers.clone()).await.ok()
        .map(|m| Arc::new(RwLock::new(m)))
} else { None };

// Pass to services
henry_http::run_server(config, &bind_addr, health, api_token, containers.clone(), shutdown_rx)
henry_telegram::run_bot(token, config, health, containers.clone(), shutdown_rx)
```

**HTTP server signature**:
```rust
pub async fn run_server(
    config: HttpConfig,
    bind_addr: &str,
    health: Arc<RwLock<HealthManager>>,
    api_token: Option<String>,
    containers: Option<Arc<RwLock<ContainerManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), HttpError>
```

**Telegram bot signature**:
```rust
pub async fn run_bot(
    token: String,
    config: TelegramConfig,
    health: Arc<RwLock<HealthManager>>,
    containers: Option<Arc<RwLock<ContainerManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), TelegramError>
```

### Binary

- Size: 10MB (release, LTO, stripped) - up from 9MB due to bollard

---

## Phase 4: Media (NEXT)

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
