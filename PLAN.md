# Henry - Implementation Plan

Personal home server daemon for Mac Mini M4 Pro. Security-first, minimal Rust implementation.

## Design Philosophy

- Dieter Rams' "less but better"
- Jony Ive's purposeful design
- Andy Matuschak's progressive disclosure

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         HENRY - Mac Mini M4 Pro                             │
├─────────────────────────────────────────────────────────────────────────────┤
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐                        │
│  │ TUI         │   │ Telegram    │   │ HTTP API    │                        │
│  │ (Ratatui)   │   │ (Teloxide)  │   │ (Axum)      │                        │
│  └──────┬──────┘   └──────┬──────┘   └──────┬──────┘                        │
│         └─────────────────┼─────────────────┘                                │
│                    ┌──────┴──────┐                                           │
│                    │ Message Bus │                                           │
│                    │ (Tokio MPSC)│                                           │
│                    └──────┬──────┘                                           │
│  ┌─────────┬─────────┬────┴────┬─────────┬─────────┬─────────┐              │
│  │         │         │         │         │         │         │              │
│  ▼         ▼         ▼         ▼         ▼         ▼         ▼              │
│ ┌────┐  ┌────┐   ┌────┐    ┌────┐    ┌────┐   ┌────┐    ┌────┐             │
│ │Srvr│  │Mdia│   │Clde│    │Mnt │    │Hlth│   │Rctv│    │Agnt│             │
│ │Mod │  │Mod │   │Mod │    │Mod │    │Mgr │   │Eng │    │Eng │             │
│ └─┬──┘  └─┬──┘   └─┬──┘    └─┬──┘    └─┬──┘   └─┬──┘    └─┬──┘             │
│   │       │        │         │         │        │         │                 │
│ Podman  Jellyfin Anthropic  Cron    Probes   Observe   ReAct               │
│         /Plex    /Claude    Backup  Alerts   →Decide   Loop                │
│                                              →Execute  Planning            │
└─────────────────────────────────────────────────────────────────────────────┘
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
│   ├── henry-maint/              # (Phase 6) Updates, backup, cleanup
│   ├── henry-reactive/           # (Phase 7) Reactive control loop
│   └── henry-agent/              # (Phase 8) Agentic task execution
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
**Completed:** 2025-02-04

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
**Completed:** 2025-02-04

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

## Phase 4: Media (COMPLETE)

**Commit:** `a98b2f33c` on branch `init`
**Completed:** 2025-02-05

### Implemented Crate: `henry-media`

**Purpose:** Jellyfin media server management via container lifecycle + REST API

### Directory Structure

```
crates/henry-media/
├── Cargo.toml
└── src/
    ├── lib.rs          # MediaManager, MediaError, public API
    ├── jellyfin.rs     # JellyfinClient, Jellyfin REST API integration
    └── types.rs        # ServerInfo, MediaLibrary, PlaybackSession, MediaStatus, Jellyfin API DTOs
```

### Key Features

1. **Jellyfin container lifecycle**: start, stop, restart, logs (delegates to `henry-server::ContainerManager`)
2. **Jellyfin REST API client**: system info, library listing, library scan, active sessions
3. **Library path validation**: Checks configured paths exist on disk
4. **Hardware transcoding status**: Reports VideoToolbox/hw acceleration availability from Jellyfin
5. **Playback monitoring**: Active sessions with progress, pause state, and transcode info
6. **Unified status**: Single `MediaStatus` combining container state, API reachability, and server info

### Config

```toml
[media]
enabled = true
library_paths = ["/Volumes/Media/Movies", "/Volumes/Media/TV"]
jellyfin_container = "jellyfin"
jellyfin_url = "http://localhost:8096"
jellyfin_api_key_ref = "op://Private/Jellyfin/api-key"
hw_transcode = true
```

### Telegram Commands

- `/media` or `/media status` - Show media module status
- `/media libraries` - List Jellyfin libraries
- `/media sessions` - Show active playback sessions
- `/media scan` - Trigger full library scan
- `/media start` - Start Jellyfin container
- `/media stop` - Stop Jellyfin container
- `/media restart` - Restart Jellyfin container
- `/media logs [tail]` - Get Jellyfin container logs

### HTTP API Endpoints

- `GET /api/media/status` - Full media module status
- `GET /api/media/libraries` - List all libraries
- `GET /api/media/sessions` - Active playback sessions
- `POST /api/media/scan` - Trigger library scan
- `POST /api/media/start` - Start Jellyfin container
- `POST /api/media/stop` - Stop Jellyfin container
- `POST /api/media/restart` - Restart Jellyfin container
- `GET /api/media/logs?tail=50` - Get Jellyfin logs

### Tests

11 new unit tests (46 total):
- `henry-media` types: 5 tests (media status, playback session, Jellyfin JSON deserialization)
- `henry-media` jellyfin: 2 tests (format_ticks, URL trimming)
- `henry-media` lib: 4 tests (manager creation, path validation, container name, status without services)

### Code Patterns (Updated)

**Service signatures now include media** (in `daemon.rs`):
```rust
// Shared media manager
let media: Option<Arc<RwLock<MediaManager>>> = if config.media.enabled {
    let api_key = resolve_jellyfin_api_key(&secrets, &config.media).await;
    let manager = MediaManager::new(config.media.clone(), containers.clone(), api_key);
    Some(Arc::new(RwLock::new(manager)))
} else { None };

// Pass to services
henry_http::run_server(config, &bind_addr, health, api_token, containers, media, shutdown_rx)
henry_telegram::run_bot(token, config, health, containers, media, shutdown_rx)
```

**HTTP server signature**:
```rust
pub async fn run_server(
    config: HttpConfig,
    bind_addr: &str,
    health: Arc<RwLock<HealthManager>>,
    api_token: Option<String>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), HttpError>
```

**Telegram bot signature**:
```rust
pub async fn run_bot(
    token: String,
    config: TelegramConfig,
    health: Arc<RwLock<HealthManager>>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), TelegramError>
```

---

## Phase 5: AI Integration (COMPLETE)

**Commit:** `218c461af` on branch `init`
**Completed:** 2025-02-05

### Implemented Crate: `henry-claude`

**Purpose:** Anthropic API client and Claude Code session management with workspace isolation

### Directory Structure

```
crates/henry-claude/
├── Cargo.toml
└── src/
    ├── lib.rs          # ClaudeManager, ClaudeError, public API
    ├── api.rs          # AnthropicClient, Messages API integration
    ├── session.rs      # SessionManager, Claude Code process spawning
    └── types.rs        # SessionInfo, SessionStatus, ClaudeStatus, Message types
```

### Key Features

1. **Anthropic API client**: Direct access to Claude via Messages API
2. **Claude Code process spawning**: Start/stop Claude Code CLI sessions
3. **Session management**: Track multiple concurrent sessions with status updates
4. **Workspace isolation**: Each session runs in its own workspace directory
5. **API key via 1Password**: Secure credential storage via `op://` references
6. **Session lifecycle tracking**: Starting, Running, Stopping, Stopped, Error states

### Config

```toml
[claude]
enabled = true
api_key_ref = "op://Private/Anthropic API/credential"
max_sessions = 4
workspace_dir = "~/claude-workspaces"
```

### Telegram Commands

- `/claude` or `/claude status` - Show Claude module status
- `/claude list` or `/claude sessions` - List all sessions
- `/claude new <workspace>` - Start new Claude Code session
- `/claude stop <id>` - Stop a session
- `/claude ask <question>` - Send a question to Claude via API

### HTTP API Endpoints

- `GET /api/claude/status` - Full Claude module status
- `GET /api/claude/sessions` - List all sessions
- `POST /api/claude/sessions/new` - Start new session (body: `{"workspace": "name"}`)
- `GET /api/claude/sessions/{id}` - Get session details
- `POST /api/claude/sessions/{id}/stop` - Stop a session
- `POST /api/claude/ask` - Send message to Claude API (body: `{"question": "...", "model": "...", "max_tokens": N}`)

### Tests

21 new unit tests (72 total):
- `henry-claude` types: 4 tests (session status, lifecycle, error state, message creation)
- `henry-claude` api: 4 tests (client creation, custom base URL, request serialization)
- `henry-claude` session: 6 tests (manager creation, workspace resolution, session operations)
- `henry-claude` lib: 7 tests (manager configuration, status, session formatting)

### Code Patterns (Updated)

**Service signatures now include claude** (in `daemon.rs`):
```rust
// Shared Claude manager
let claude: Option<Arc<RwLock<ClaudeManager>>> = if config.claude.enabled {
    let api_key = match secrets.get(&config.claude.api_key_ref).await {
        Ok(key) => Some(key),
        Err(e) => { warn!("Failed to get Anthropic API key: {}", e); None }
    };
    let manager = ClaudeManager::new(config.claude.clone(), api_key);
    Some(Arc::new(RwLock::new(manager)))
} else { None };

// Pass to services
henry_http::run_server(config, &bind_addr, health, api_token, containers, media, claude, shutdown_rx)
henry_telegram::run_bot(token, config, health, containers, media, claude, shutdown_rx)
```

**HTTP server signature**:
```rust
pub async fn run_server(
    config: HttpConfig,
    bind_addr: &str,
    health: Arc<RwLock<HealthManager>>,
    api_token: Option<String>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), HttpError>
```

**Telegram bot signature**:
```rust
pub async fn run_bot(
    token: String,
    config: TelegramConfig,
    health: Arc<RwLock<HealthManager>>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), TelegramError>
```

### Binary

- Size: 12MB (release, LTO, stripped) - up from 10MB due to uuid, which, serde_json deps

---

## Phase 6: Self-Maintenance (COMPLETE)

**Commit:** `1633d9aa9` on branch `init`
**Completed:** 2025-02-05

### Implemented Crate: `henry-maint`

**Purpose:** Automated maintenance tasks: backups, log rotation, update checking

### Directory Structure

```
crates/henry-maint/
├── Cargo.toml
└── src/
    ├── lib.rs          # MaintenanceManager, MaintError, public API
    ├── backup.rs       # BackupManager, tar.gz archive creation
    ├── logs.rs         # LogRotator, log file rotation with compression
    ├── scheduler.rs    # MaintScheduler, cron-based job scheduling
    ├── update.rs       # UpdateChecker, GitHub release checking
    └── types.rs        # MaintStatus, BackupInfo, ScheduledJob, UpdateInfo
```

### Key Features

1. **Cron scheduler**: `tokio-cron-scheduler` for automated tasks
2. **Automated backups**: Compress state DB and config to tar.gz archives
3. **Log rotation**: Rotate and compress logs that exceed size threshold
4. **Update checking**: Check GitHub releases for newer versions
5. **Configurable retention**: Max backups and max log files settings

### Config

```toml
[maint]
enabled = true
backup_enabled = true
backup_cron = "0 0 3 * * *"         # 3 AM daily
backup_dir = "~/.henry/backups"
max_backups = 7
log_rotation_enabled = true
log_rotation_cron = "0 0 0 * * *"   # Midnight daily
log_dir = "~/.henry/logs"
max_log_files = 10
max_log_size = 10485760             # 10 MB
update_check_enabled = true
update_check_cron = "0 0 12 * * *"  # Noon daily
github_repo = "specbug/henry"
```

### Telegram Commands

- `/maint` or `/maint status` - Show maintenance module status
- `/maint backup` - Create a backup now
- `/maint backups` - List existing backups
- `/maint jobs` - Show scheduled jobs
- `/maint rotate` - Rotate logs now
- `/maint update` - Check for updates

### HTTP API Endpoints

- `GET /api/maint/status` - Full maintenance module status
- `GET /api/maint/backups` - List all backups
- `POST /api/maint/backups/create` - Create backup now
- `DELETE /api/maint/backups/{name}` - Delete a backup
- `GET /api/maint/jobs` - List scheduled jobs
- `POST /api/maint/rotate` - Trigger log rotation
- `POST /api/maint/update-check` - Check for updates

### Tests

23 new unit tests (95 total):
- `henry-maint` types: 4 tests (status, job types, serialization)
- `henry-maint` backup: 5 tests (create, list, cleanup, format size)
- `henry-maint` logs: 4 tests (rotation, find files, total size)
- `henry-maint` scheduler: 4 tests (creation, add/remove jobs)
- `henry-maint` update: 5 tests (version comparison, checker)
- `henry-maint` lib: 1 test (format_job)

### Binary

- Size: 13MB (release, LTO, stripped) - up from 12MB due to tar/flate2/cron deps

### Code Patterns (Updated)

**Service signatures now include maint** (in `daemon.rs`):
```rust
// Shared maintenance manager
let maint: Option<Arc<RwLock<MaintenanceManager>>> = if config.maint.enabled {
    let manager = MaintenanceManager::new(config.maint.clone());
    Some(Arc::new(RwLock::new(manager)))
} else { None };

// Pass to services
henry_http::run_server(config, &bind_addr, health, api_token, containers, media, claude, maint, shutdown_rx)
henry_telegram::run_bot(token, config, health, containers, media, claude, maint, shutdown_rx)
```

**HTTP server signature**:
```rust
pub async fn run_server(
    config: HttpConfig,
    bind_addr: &str,
    health: Arc<RwLock<HealthManager>>,
    api_token: Option<String>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
    maint: Option<Arc<RwLock<MaintenanceManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), HttpError>
```

**Telegram bot signature**:
```rust
pub async fn run_bot(
    token: String,
    config: TelegramConfig,
    health: Arc<RwLock<HealthManager>>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
    maint: Option<Arc<RwLock<MaintenanceManager>>>,  // NEW
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), TelegramError>
```

---

## Phase 7: Reactive Control Loop (COMPLETE)

**Commit:** `e49302abb` on branch `init`
**Completed:** 2025-02-05

### Implemented Crate: `henry-reactive`

**Purpose:** Continuous monitoring, anomaly detection, auto-remediation, and escalation

### Directory Structure

```
crates/henry-reactive/
├── Cargo.toml
└── src/
    ├── lib.rs          # ReactiveEngine, start/stop, status
    ├── observer.rs     # AnomalyObserver, metric collection
    ├── decision.rs     # DecisionEngine, policy matching
    ├── action.rs       # ActionExecutor, remediation execution
    ├── policy.rs       # RemediationPolicy, rules, cooldowns
    └── types.rs        # Anomaly, Action, SystemEvent, Severity
```

### Control Loop Pattern

```
1. OBSERVE   → Collect metrics from HealthManager, poll container states
2. DETECT    → Compare against thresholds, emit SystemEvents
3. DECIDE    → Match events to policy rules, check cooldowns/rate limits
4. EXECUTE   → Run auto-approved actions with timeout
5. VERIFY    → Confirm fix worked (re-check metrics after delay)
6. ESCALATE  → If failed or needs approval → notify user
```

### Key Features

1. **Anomaly detection**: Disk, memory, CPU thresholds; container crashes; service unreachable
2. **Auto-remediation**: Restart containers, rotate logs, trigger backups
3. **Rate limiting**: Max actions per hour, max concurrent actions
4. **Quiet hours**: Suppress non-critical actions during configurable hours
5. **Approval workflow**: Some actions require human approval before execution
6. **Cooldown periods**: Prevent repeated triggering of same rule

### System Events

| Event | Severity | Auto-action |
|-------|----------|-------------|
| `container_crashed` | Critical | Restart container |
| `disk_space_low` | Warning/Critical | Rotate logs |
| `service_unreachable` | Critical | Restart + notify |
| `memory_pressure` | Warning/Critical | Notify user |
| `backup_failed` | Warning | Retry backup |
| `update_available` | Info | Notify user |
| `container_restart_loop` | Critical | Escalate to human |
| `high_cpu_usage` | Warning | Notify user |
| `module_unhealthy` | Warning | Notify user |

### Config

```toml
[reactive]
enabled = true
check_interval_secs = 10
max_actions_per_hour = 20
max_concurrent_actions = 3
quiet_hours_start = 2
quiet_hours_end = 6

[reactive.thresholds]
disk_warning_percent = 80.0
disk_critical_percent = 95.0
memory_warning_percent = 85.0
memory_critical_percent = 95.0
cpu_warning_percent = 90.0
cpu_alert_duration_secs = 300

[reactive.notifications]
verbosity = "all"           # all, milestones, critical
include_routine_fixes = true
batch_interval_secs = 0     # 0 = immediate
```

### Telegram Commands

- `/reactive` or `/reactive status` - Show reactive engine status
- `/reactive pause` - Pause the control loop
- `/reactive resume` - Resume the control loop
- `/reactive history [N]` - Show last N actions (default: 10)
- `/reactive anomalies` - List active anomalies
- `/reactive pending` - Show actions awaiting approval
- `/reactive approve <id>` - Approve a pending action

### HTTP API Endpoints

- `GET /api/reactive/status` - Engine status (running, paused, counts)
- `POST /api/reactive/pause` - Pause the engine
- `POST /api/reactive/resume` - Resume the engine
- `GET /api/reactive/anomalies` - List active anomalies
- `GET /api/reactive/actions` - List all actions
- `POST /api/reactive/actions/{id}/approve` - Approve an action (body: `{"approved_by": "user"}`)

### Tests

24 new unit tests (119 total):
- `henry-reactive` types: 6 tests (severity ordering, event types, anomaly lifecycle, action lifecycle)
- `henry-reactive` policy: 6 tests (rule matching, cooldowns, quiet hours)
- `henry-reactive` observer: 4 tests (threshold detection, metric collection)
- `henry-reactive` decision: 4 tests (policy evaluation, rate limiting)
- `henry-reactive` lib: 2 tests (engine creation, start/stop)
- Integration tests in `henry-telegram` and `henry-http`

---

## Phase 8: Agentic Task Execution (COMPLETE)

**Commit:** `e49302abb` on branch `init`
**Completed:** 2025-02-05

### Implemented Crate: `henry-agent`

**Purpose:** High-level task execution using ReAct pattern (Thought → Action → Observation)

### Directory Structure

```
crates/henry-agent/
├── Cargo.toml
└── src/
    ├── lib.rs          # AgentEngine, task submission, lifecycle
    ├── planner.rs      # TaskPlanner, Claude-powered step generation
    ├── executor.rs     # ReActExecutor, step execution with retries
    ├── evaluator.rs    # ResultEvaluator, success/failure assessment
    ├── task.rs         # TaskQueue, persistence, priority ordering
    └── types.rs        # Task, Step, StepType, TaskStatus, AgentState
```

### Execution Flow

```
1. SUBMIT   → Task added to priority queue, persisted to SQLite
2. PLAN     → Claude generates execution steps from description
3. EXECUTE  → ReAct loop: Thought → Action → Observation
4. EVALUATE → Assess success, generate summary
5. COMPLETE → Update status, notify user
```

### Key Features

1. **Claude-powered planning**: Automatically generates steps from task description
2. **ReAct pattern**: Each step iterates Thought → Action → Observation
3. **Multiple step types**: Claude Code, Claude API, shell commands, container ops
4. **Token budgeting**: Per-task token limits prevent runaway costs
5. **User input requests**: Steps can pause for user input
6. **Priority queue**: Critical > High > Normal > Low
7. **Persistence**: Tasks survive daemon restarts via SQLite

### Step Types

| Type | Description |
|------|-------------|
| `ClaudeCode` | Execute Claude Code session in a workspace |
| `ClaudeApi` | Call Claude API directly |
| `Command` | Run a shell command |
| `ContainerOp` | Start/stop/restart a container |
| `UserInput` | Pause and wait for user input |

### Task Lifecycle

```
Queued → Planning → Executing → Completed
                 ↘ AwaitingInput ↗
                 ↘ Failed
                 ↘ Cancelled
```

### Config

```toml
[agent]
enabled = false
max_concurrent_tasks = 1
max_steps_per_task = 10
max_tokens_per_task = 50000
max_iterations_per_step = 5
step_timeout_secs = 300
planning_model = "claude-sonnet-4-20250514"

[agent.safety]
command_allowlist = ["ls", "cat", "git", "npm", "cargo"]
command_blocklist = ["rm -rf", "sudo", "chmod 777"]
require_approval_for = ["file_deletion", "system_changes"]
```

### Telegram Commands

- `/agent` or `/agent status` - Show agent status
- `/agent new <title> - <description>` - Submit a new task
- `/agent list` - List all tasks
- `/agent task <id>` - Show task details and steps
- `/agent cancel <id>` - Cancel a task
- `/agent input <id> <key>=<value>` - Provide input for waiting task
- `/agent pause` - Pause the agent
- `/agent resume` - Resume the agent

### HTTP API Endpoints

- `GET /api/agent/status` - Agent status (state, pending count)
- `GET /api/agent/tasks` - List all tasks
- `POST /api/agent/tasks` - Create task (body: `{"title": "...", "description": "...", "priority": "normal"}`)
- `GET /api/agent/tasks/{id}` - Get task details
- `POST /api/agent/tasks/{id}/cancel` - Cancel a task
- `POST /api/agent/tasks/{id}/input` - Provide input (body: `{"key": "...", "value": "..."}`)
- `POST /api/agent/pause` - Pause the agent
- `POST /api/agent/resume` - Resume the agent

### Tests

Tests included in Phase 7 & 8 commit:
- `henry-agent` types: Task, Step, StepType serialization
- `henry-agent` task: Queue operations, persistence
- `henry-agent` executor: ReAct iteration, command safety
- `henry-agent` planner: Step generation mocking

### Binary

- Size: 14MB (release, LTO, stripped) - up from 13MB due to uuid deps for task IDs

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
