# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Henry is a security-first Rust daemon for managing home server functions on Mac Mini M4 Pro. It provides container management, Telegram bot control, HTTP API, media server integration (Jellyfin), AI integration (Claude), automated maintenance, reactive monitoring, and agentic task execution.

## Build and Development Commands

```bash
# Build
cargo build              # Debug build
cargo build --release    # Release build (14MB, LTO + stripped)

# Test
cargo test               # Run all tests (~120 tests)
cargo test -p henry-config  # Test specific crate

# Run
./target/release/henry run              # Start with TUI
./target/release/henry run --headless   # Start without TUI (for services)
./target/release/henry status           # Check if daemon is running
./target/release/henry check            # Validate configuration
./target/release/henry init             # Generate config.toml
./target/release/henry info             # Show version and features

# With options
./target/release/henry --config path/to/config.toml run
./target/release/henry --verbose run    # Debug logging
```

## Architecture

### Workspace Structure

The project is a Cargo workspace with 19 crates in `crates/`:

| Crate | Purpose |
|-------|---------|
| `henry-core` | Main binary, daemon loop, CLI (clap), signal handling |
| `henry-config` | TOML configuration parsing, hot reload via notify |
| `henry-lock` | Single-instance locking (port binding + PID file) |
| `henry-secrets` | 1Password CLI (`op://`) secret resolution |
| `henry-state` | SQLite persistence (sqlx) |
| `henry-health` | System metrics via sysinfo |
| `henry-tui` | Ratatui terminal interface |
| `henry-telegram` | Telegram bot (Teloxide) with allowlist auth |
| `henry-http` | Axum REST API with Bearer token auth, WebSocket events |
| `henry-server` | Podman/Docker container management (bollard) |
| `henry-media` | Jellyfin media server integration |
| `henry-claude` | Anthropic API client + Claude Code session spawning |
| `henry-maint` | Automated backups, log rotation, update checking |
| `henry-reactive` | Observe→Decide→Execute control loop, anomaly detection |
| `henry-agent` | ReAct pattern task execution with Claude planning |
| `henry-proxy` | Caddy reverse proxy management, dynamic route configuration |
| `henry-apps` | App deployment from GitHub repos, Dockerfile generation |
| `henry-tunnel` | Cloudflare Tunnel (cloudflared) management, DNS integration |

### Key Design Patterns

**Shared state via `Arc<RwLock<T>>`**: All managers are wrapped and passed to services:
```rust
let health: Arc<RwLock<HealthManager>> = Arc::new(RwLock::new(HealthManager::new(version)));
let containers: Option<Arc<RwLock<ContainerManager>>> = ...;
```

**Graceful shutdown via broadcast channel**:
```rust
let (shutdown_tx, _) = broadcast::channel(1);
let shutdown_rx = shutdown_tx.subscribe();
tokio::spawn(async move {
    service::run(..., shutdown_rx).await
});
// On shutdown:
let _ = shutdown_tx.send(());
```

**Service function signature pattern** (all services follow this):
```rust
pub async fn run_server(
    config: HttpConfig,
    bind_addr: &str,
    health: Arc<RwLock<HealthManager>>,
    api_token: Option<String>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
    maint: Option<Arc<RwLock<MaintenanceManager>>>,
    reactive: Option<Arc<RwLock<ReactiveEngine>>>,
    agent: Option<Arc<RwLock<AgentEngine>>>,
    event_tx: broadcast::Sender<WsEvent>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), HttpError>
```

**WebSocket event broadcasting**:
```rust
// Event bus created alongside shutdown channel
let (shutdown_tx, _) = broadcast::channel(1);
let (event_tx, _) = broadcast::channel::<henry_http::WsEvent>(256);

// Broadcast events from anywhere
let _ = event_tx.send(WsEvent::HealthUpdate { ... });
```

**Module registration in `daemon.rs`**:
```rust
let modules = [
    ("server", config.containers.enabled),
    ("media", config.media.enabled),
    ("claude", config.claude.enabled),
    // ... etc
];
```

### Security Patterns

- **Timing-safe comparison**: Use `subtle::ConstantTimeEq` for tokens/secrets
- **File permissions**: 0o600 for files, 0o700 for directories
- **Allowlist auth**: Telegram uses user ID/username allowlists, HTTP uses Bearer tokens
- **Secret filtering**: Container inspect hides sensitive env vars
- **Single-instance locking**: Port binding + PID file with stale detection

### Signal Handling

- `SIGTERM` / `SIGINT`: Graceful shutdown
- `SIGUSR1` / `SIGHUP`: Configuration reload

### Runtime Paths

```
~/.henry/
├── config.toml      # Configuration
├── henry.pid        # PID file
├── state/henry.db   # SQLite state
├── logs/            # Log files
├── backups/         # Automated backups
└── cache/           # Temporary cache
```

## Configuration

Secrets use 1Password CLI references (`op://vault/item/field`). See `config.example.toml` for all options.

Key sections: `[daemon]`, `[network]`, `[telegram]`, `[http]`, `[containers]`, `[media]`, `[claude]`, `[maint]`, `[reactive]`, `[agent]`, `[proxy]`, `[apps]`, `[tunnel]`

## Adding New Modules

When adding a new module/manager:

1. Create crate in `crates/henry-<name>/`
2. Add to workspace in root `Cargo.toml`
3. Add config struct to `henry-config/src/lib.rs`
4. Initialize in `henry-core/src/daemon.rs` (follow existing pattern)
5. Add as dependency to `henry-http` and `henry-telegram`
6. Add HTTP endpoints in `henry-http/src/lib.rs`
7. Add Telegram commands in `henry-telegram/src/lib.rs`
8. Update module registration arrays in `daemon.rs`
