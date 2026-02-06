# Henry

A security-first Rust daemon for managing your home server. Built for Mac Mini M4 Pro.

## What is Henry?

Henry is a unified daemon that manages all aspects of a home server:

- **Container Management** - Start, stop, restart, and monitor Docker/Podman containers
- **Media Server** - Control Jellyfin, trigger library scans, view active playback sessions
- **AI Integration** - Spawn Claude Code sessions and query the Anthropic API directly
- **App Deployment (PaaS)** - Deploy single or multi-service apps from GitHub repos with automatic service discovery, inter-service networking, and SSL
- **Automated Maintenance** - Scheduled backups, log rotation, update checking
- **Reactive Monitoring** - Automatic anomaly detection and self-healing (e.g., restart crashed containers)
- **Agentic Tasks** - Submit complex tasks that Claude plans and executes step-by-step

You interact with Henry via:
- **Terminal UI** - Real-time dashboard when running in foreground
- **Telegram Bot** - Control your server from anywhere
- **HTTP API** - RESTful endpoints for automation/scripting
- **WebSocket** - Real-time event streaming for dashboards

---

## Quick Start

### Build

```bash
cargo build --release
```

Binary: `./target/release/henry` (~14MB)

### Configure

```bash
# Generate default config
./target/release/henry init

# Edit configuration
vim ~/.henry/config.toml
```

### Run

```bash
# Interactive mode with TUI
./target/release/henry run

# Headless (for launchd/systemd)
./target/release/henry run --headless

# Check if already running
./target/release/henry status
```

---

## Configuration

Henry uses TOML configuration at `~/.henry/config.toml`. Secrets are stored in 1Password and referenced via `op://vault/item/field` format.

### Minimal Config

```toml
[daemon]
data_dir = "~/.henry"

[http]
enabled = true
port = 18791

[containers]
enabled = true
use_podman = true
```

### Full Config Reference

```toml
[daemon]
data_dir = "~/.henry"
pid_file = "~/.henry/henry.pid"
lock_port = 18790

[network]
bind_address = "0.0.0.0"
# static_ip = "192.168.1.100"

[network.tailscale]
enabled = false
hostname = "henry"
serve_https = true

[telegram]
enabled = false
token_ref = "op://Private/Henry Telegram Bot/credential"
allowed_users = []          # Telegram user IDs (numbers)
allowed_usernames = []      # Telegram usernames (strings)

[http]
enabled = true
port = 18791
# token_ref = "op://Private/Henry API/token"   # Optional: require Bearer token

[containers]
enabled = true
use_podman = true
# socket_path = ""          # Auto-detected if empty
allowed_containers = []     # Empty = all containers allowed

[media]
enabled = false
library_paths = []
jellyfin_container = "jellyfin"
jellyfin_url = "http://localhost:8096"
# jellyfin_api_key_ref = "op://Private/Jellyfin/api-key"
hw_transcode = true

[claude]
enabled = false
api_key_ref = "op://Private/Anthropic API/credential"
max_sessions = 4
workspace_dir = "~/claude-workspaces"

[maint]
enabled = true
backup_enabled = true
backup_cron = "0 0 3 * * *"           # 3 AM daily
backup_dir = "~/.henry/backups"
max_backups = 7
log_rotation_enabled = true
log_rotation_cron = "0 0 0 * * *"     # Midnight daily
log_dir = "~/.henry/logs"
max_log_files = 10
max_log_size = 10485760               # 10 MB
update_check_enabled = true
update_check_cron = "0 0 12 * * *"    # Noon daily
github_repo = "specbug/henry"

[reactive]
enabled = false
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

[agent]
enabled = false
max_concurrent_tasks = 1
max_steps_per_task = 10
max_tokens_per_task = 50000
planning_model = "claude-sonnet-4-20250514"

[agent.safety]
command_allowlist = ["ls", "cat", "git", "npm", "cargo"]
command_blocklist = ["rm -rf", "sudo", "chmod 777"]

[proxy]
enabled = false
caddy_image = "caddy:2-alpine"
caddy_container = "henry-caddy"
http_port = 80
https_port = 443
config_dir = "~/.henry/caddy"
# acme_email = "you@example.com"
network_name = "henry-apps"

[apps]
enabled = false
workspace_dir = "~/.henry/apps"
network_name = "henry-apps"
auto_restart = true
health_check_interval_secs = 30
default_port = 8080
max_concurrent_builds = 2
build_timeout_secs = 600

[tunnel]
enabled = false
tunnel_token_ref = "op://Private/Cloudflare Tunnel/token"
api_token_ref = "op://Private/Cloudflare/api-token"
zone_id = ""
tunnel_name = "henry-tunnel"
container_name = "henry-cloudflared"
container_image = "cloudflare/cloudflared:latest"
network_name = "henry-apps"

[logging]
level = "info"
dir = "~/.henry/logs"
json = false
```

### Secret Management (1Password)

Henry never stores secrets in config files. Use 1Password CLI:

```bash
# Install 1Password CLI
brew install 1password-cli

# Sign in
op signin

# Store a secret
op item create --category=login --title="Henry Telegram Bot" credential=YOUR_BOT_TOKEN

# Reference in config.toml
token_ref = "op://Private/Henry Telegram Bot/credential"
```

---

## CLI Commands

```bash
henry run                    # Start daemon with TUI
henry run --headless         # Start without TUI (for services)
henry status                 # Check if daemon is running
henry check                  # Validate configuration
henry init                   # Generate default config.toml
henry info                   # Show version and features

# App deployment
henry deploy <repo-url>                 # Deploy from GitHub
henry deploy <repo-url> --domain example.com --path /app/myapp
henry apps                              # List deployed apps
henry logs <name>                       # App logs
henry logs <name> --service api         # Service-specific logs (multi-service apps)

# macOS LaunchAgent
henry install                # Install as LaunchAgent (auto-starts on login)
henry install --force        # Reinstall
henry uninstall              # Remove LaunchAgent

# Options
henry --config <path> run    # Use specific config file
henry --verbose run          # Enable debug logging
```

### Signal Handling

| Signal | Action |
|--------|--------|
| `SIGTERM` / `SIGINT` | Graceful shutdown |
| `SIGUSR1` / `SIGHUP` | Reload configuration |

---

## Terminal UI (TUI)

When running with `henry run` (without `--headless`), you get an interactive dashboard.

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Tab` | Switch views |
| `1-4` | Jump to view (Dashboard, Modules, Containers, Logs) |
| `F1` | Help |
| `F5` | Refresh metrics |
| `:` | Command mode |
| `q` | Quit |

---

## Telegram Bot

Add your Telegram user ID or username to `allowed_users` / `allowed_usernames` in config, then interact with your bot.

### Commands

| Command | Description |
|---------|-------------|
| `/help` | Show all commands |
| `/status` | System status (uptime, CPU, memory) |
| `/modules` | Module health status |
| `/metrics` | Detailed system metrics |

#### Containers

| Command | Description |
|---------|-------------|
| `/containers` | List all containers |
| `/container start <name>` | Start a container |
| `/container stop <name>` | Stop a container |
| `/container restart <name>` | Restart a container |
| `/container logs <name> [N]` | Last N lines of logs (default: 50) |

#### Media (Jellyfin)

| Command | Description |
|---------|-------------|
| `/media` or `/media status` | Media module status |
| `/media libraries` | List Jellyfin libraries |
| `/media sessions` | Active playback sessions |
| `/media scan` | Trigger library scan |
| `/media start` | Start Jellyfin container |
| `/media stop` | Stop Jellyfin container |
| `/media restart` | Restart Jellyfin container |
| `/media logs [N]` | Jellyfin container logs |

#### Claude AI

| Command | Description |
|---------|-------------|
| `/claude` or `/claude status` | Claude module status |
| `/claude list` | List active Claude Code sessions |
| `/claude new <workspace>` | Start new Claude Code session |
| `/claude stop <id>` | Stop a session |
| `/claude ask <question>` | Query Claude API directly |

#### Maintenance

| Command | Description |
|---------|-------------|
| `/maint` or `/maint status` | Maintenance status |
| `/maint backup` | Create backup now |
| `/maint backups` | List existing backups |
| `/maint jobs` | Show scheduled jobs |
| `/maint rotate` | Rotate logs now |
| `/maint update` | Check for updates |

#### Reactive Engine

| Command | Description |
|---------|-------------|
| `/reactive` or `/reactive status` | Engine status |
| `/reactive pause` | Pause monitoring |
| `/reactive resume` | Resume monitoring |
| `/reactive history [N]` | Last N actions (default: 10) |
| `/reactive anomalies` | Active anomalies |
| `/reactive pending` | Actions awaiting approval |
| `/reactive approve <id>` | Approve a pending action |

#### Agent Tasks

| Command | Description |
|---------|-------------|
| `/agent` or `/agent status` | Agent status |
| `/agent new <title> - <desc>` | Submit a new task |
| `/agent list` | List all tasks |
| `/agent task <id>` | Task details and progress |
| `/agent cancel <id>` | Cancel a task |
| `/agent input <id> <key>=<val>` | Provide input for waiting task |
| `/agent pause` | Pause task processing |
| `/agent resume` | Resume task processing |

#### Apps (PaaS)

| Command | Description |
|---------|-------------|
| `/app list` | List all deployed apps |
| `/app deploy <repo> to <domain/path>` | Deploy app from GitHub |
| `/app status <name>` | App status and health |
| `/app logs <name> [N]` | App container logs |
| `/app stop <name>` | Stop an app |
| `/app start <name>` | Start an app |
| `/app remove <name>` | Remove an app |
| `/app redeploy <name>` | Redeploy an app |

#### Proxy

| Command | Description |
|---------|-------------|
| `/proxy status` | Caddy proxy status |
| `/proxy routes` | List all routes |

#### Tunnel

| Command | Description |
|---------|-------------|
| `/tunnel status` | Cloudflare Tunnel status |

---

## HTTP API

Base URL: `http://localhost:18791`

If `token_ref` is configured, include header: `Authorization: Bearer <token>`

### General

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/` | API info |
| GET | `/health` | Health check (no auth required) |
| GET | `/api/status` | System status |
| GET | `/api/modules` | Module health |
| GET | `/api/metrics` | System metrics |
| GET | `/api/snapshot` | Full health snapshot |

### Containers

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/containers` | List all containers |
| GET | `/api/containers/{name}` | Container details |
| POST | `/api/containers/{name}/start` | Start container |
| POST | `/api/containers/{name}/stop` | Stop container |
| POST | `/api/containers/{name}/restart` | Restart container |
| GET | `/api/containers/{name}/logs?tail=50` | Container logs |

### Media

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/media/status` | Media module status |
| GET | `/api/media/libraries` | List libraries |
| GET | `/api/media/sessions` | Active playback sessions |
| POST | `/api/media/scan` | Trigger library scan |
| POST | `/api/media/start` | Start Jellyfin |
| POST | `/api/media/stop` | Stop Jellyfin |
| POST | `/api/media/restart` | Restart Jellyfin |
| GET | `/api/media/logs?tail=50` | Jellyfin logs |

### Claude

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/claude/status` | Claude module status |
| GET | `/api/claude/sessions` | List sessions |
| POST | `/api/claude/sessions/new` | Start session |
| GET | `/api/claude/sessions/{id}` | Session details |
| POST | `/api/claude/sessions/{id}/stop` | Stop session |
| POST | `/api/claude/ask` | Query Claude |

**Request bodies:**

```bash
# Start session
curl -X POST http://localhost:18791/api/claude/sessions/new \
  -H "Content-Type: application/json" \
  -d '{"workspace": "my-project"}'

# Ask Claude
curl -X POST http://localhost:18791/api/claude/ask \
  -H "Content-Type: application/json" \
  -d '{"question": "What is 2+2?", "model": "claude-sonnet-4-20250514", "max_tokens": 1024}'
```

### Maintenance

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/maint/status` | Maintenance status |
| GET | `/api/maint/backups` | List backups |
| POST | `/api/maint/backups/create` | Create backup |
| DELETE | `/api/maint/backups/{name}` | Delete backup |
| GET | `/api/maint/jobs` | Scheduled jobs |
| POST | `/api/maint/rotate` | Rotate logs |
| POST | `/api/maint/update-check` | Check for updates |

### Reactive

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/reactive/status` | Engine status |
| POST | `/api/reactive/pause` | Pause engine |
| POST | `/api/reactive/resume` | Resume engine |
| GET | `/api/reactive/anomalies` | All anomalies |
| GET | `/api/reactive/actions` | All actions |
| POST | `/api/reactive/actions/{id}/approve` | Approve action |

**Request body for approve:**
```json
{"approved_by": "admin"}
```

### Agent

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/agent/status` | Agent status |
| GET | `/api/agent/tasks` | List tasks |
| POST | `/api/agent/tasks` | Create task |
| GET | `/api/agent/tasks/{id}` | Task details |
| POST | `/api/agent/tasks/{id}/cancel` | Cancel task |
| POST | `/api/agent/tasks/{id}/input` | Provide input |
| POST | `/api/agent/pause` | Pause agent |
| POST | `/api/agent/resume` | Resume agent |

**Request bodies:**

```bash
# Create task
curl -X POST http://localhost:18791/api/agent/tasks \
  -H "Content-Type: application/json" \
  -d '{"title": "Optimize database", "description": "Analyze slow queries and add indexes", "priority": "high"}'

# Provide input
curl -X POST http://localhost:18791/api/agent/tasks/abc123/input \
  -H "Content-Type: application/json" \
  -d '{"key": "confirm", "value": "yes"}'
```

### Apps (PaaS)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/apps` | List all apps |
| POST | `/api/apps` | Deploy new app |
| GET | `/api/apps/{id}` | App details |
| DELETE | `/api/apps/{id}` | Remove app |
| POST | `/api/apps/{id}/start` | Start app |
| POST | `/api/apps/{id}/stop` | Stop app |
| POST | `/api/apps/{id}/redeploy` | Redeploy app |
| GET | `/api/apps/{id}/logs` | App logs |
| GET | `/api/apps/{id}/health` | App health |

**Request bodies:**

**Request bodies:**

```bash
# Deploy app (auto-detects single vs multi-service)
curl -X POST http://localhost:18791/api/apps \
  -H "Content-Type: application/json" \
  -d '{"name": "myapp", "repo_url": "https://github.com/user/myapp", "domain": "example.com", "path_prefix": "/myapp"}'
```

### Multi-Service App Deployment

Henry automatically detects monorepo structures and deploys multi-service apps:

```bash
# Deploy a monorepo with frontend + backend
henry deploy https://github.com/user/odyssey --domain example.com --path /app/odyssey

# Output:
#   Successfully deployed!
#     Services:
#       api        -> henry-app-odyssey-api:8000    (example.com/app/odyssey/api)
#       webapp     -> henry-app-odyssey-webapp:3000  (example.com/app/odyssey)

# Per-service logs
henry logs odyssey --service api
henry logs odyssey --service webapp
```

Henry discovers services by analyzing the repo structure (detecting `apps/`, `packages/`, `services/` directories) and uses Claude to classify them as frontend/backend/worker services. Inter-service networking is automatically configured via Docker environment variables (`SERVICE_API_URL`, `API_URL`, etc.).

### Proxy

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/proxy/status` | Proxy status |
| GET | `/api/proxy/routes` | List routes |
| POST | `/api/proxy/routes` | Add route |
| DELETE | `/api/proxy/routes/{id}` | Remove route |
| POST | `/api/proxy/reload` | Reload Caddy |

### Tunnel

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/tunnel/status` | Tunnel status |
| GET | `/api/tunnel/routes` | List tunnel routes |
| POST | `/api/tunnel/restart` | Restart cloudflared |

---

## WebSocket

Connect to `ws://localhost:18791/ws` for real-time events.

If authentication is enabled, pass token as query param: `ws://localhost:18791/ws?token=YOUR_TOKEN`

### Events (Server → Client)

```json
{"type": "connected", "client_id": "abc123"}

{"type": "health_update", "cpu_percent": 12.5, "memory_percent": 45.2, "disk_percent": 67.8, "uptime_secs": 3600}

{"type": "container_state_changed", "name": "jellyfin", "old_state": "running", "new_state": "exited"}

{"type": "anomaly_detected", "id": "...", "event_type": "disk_space_low", "severity": "warning"}

{"type": "anomaly_resolved", "id": "..."}

{"type": "action_executed", "id": "...", "action_type": "restart_container", "success": true}

{"type": "agent_task_update", "task_id": "...", "status": "executing", "progress": "2/5"}

{"type": "pong"}
```

### Messages (Client → Server)

```json
{"type": "ping"}
```

### Example

```bash
# Install websocat
brew install websocat

# Connect
websocat ws://localhost:18791/ws
```

---

## Modules Overview

| Module | Purpose | Config Section |
|--------|---------|----------------|
| **containers** | Manage Docker/Podman containers | `[containers]` |
| **media** | Jellyfin integration | `[media]` |
| **claude** | Claude API + Claude Code sessions | `[claude]` |
| **maint** | Backups, log rotation, updates | `[maint]` |
| **reactive** | Anomaly detection & auto-remediation | `[reactive]` |
| **agent** | AI-powered task execution | `[agent]` |
| **apps** | Deploy single/multi-service apps from GitHub repos | `[apps]` |
| **proxy** | Caddy reverse proxy management | `[proxy]` |
| **tunnel** | Cloudflare Tunnel for external access | `[tunnel]` |

### Reactive Engine

The reactive engine continuously monitors your system and automatically responds to issues:

| Event | Severity | Auto-Action |
|-------|----------|-------------|
| Container crashed | Critical | Restart container |
| Disk space low | Warning/Critical | Rotate logs |
| Service unreachable | Critical | Restart + notify |
| Memory pressure | Warning/Critical | Notify |
| Backup failed | Warning | Retry backup |
| Update available | Info | Notify |
| Container restart loop | Critical | Escalate to human |

Features:
- **Rate limiting**: Max actions per hour to prevent storms
- **Quiet hours**: Suppress non-critical actions at night
- **Approval workflow**: Some actions require human approval
- **Cooldown periods**: Prevent repeated triggering

### Agent Task Execution

Submit complex tasks and let Claude plan and execute them:

```bash
# Via Telegram
/agent new Optimize database - Analyze slow queries and add indexes

# Via HTTP API
curl -X POST http://localhost:18791/api/agent/tasks \
  -H "Content-Type: application/json" \
  -d '{"title": "Optimize database", "description": "Analyze slow queries and add indexes"}'
```

The agent will:
1. **Plan** - Claude generates execution steps from your description
2. **Execute** - Each step runs using ReAct pattern (Thought → Action → Observation)
3. **Iterate** - Retries and adapts based on observations
4. **Report** - Provides results and summary

Step types: Claude Code sessions, Claude API calls, shell commands, container operations.

---

## Directory Structure

```
~/.henry/
├── config.toml          # Configuration
├── henry.pid            # PID file (when running)
├── state/
│   └── henry.db         # SQLite state database
├── logs/                # Log files
├── backups/             # Automated backups
└── cache/               # Temporary cache
```

---

## Running as a Service (macOS)

Henry has built-in LaunchAgent support. No need to manually create plist files:

```bash
# Install as LaunchAgent (auto-starts on login, auto-restarts on crash)
henry install

# Check status (shows LaunchAgent status)
henry status

# Uninstall
henry uninstall
```

The `install` command automatically:
- Detects the henry binary path
- Generates a LaunchAgent plist at `~/Library/LaunchAgents/com.henry.daemon.plist`
- Configures `RunAtLoad` and `KeepAlive`
- Sets up stdout/stderr logging to `~/.henry/logs/`
- Loads the agent via `launchctl`

For manual management:
```bash
# Force reinstall
henry install --force

# Use a specific config
henry --config /path/to/config.toml install
```

---

## Architecture

```
henry/
├── crates/
│   ├── henry-core/      # Daemon loop, CLI, signals
│   ├── henry-config/    # TOML parsing, hot reload
│   ├── henry-lock/      # Single-instance (port + PID)
│   ├── henry-secrets/   # 1Password CLI integration
│   ├── henry-state/     # SQLite state
│   ├── henry-health/    # Health probes, metrics
│   ├── henry-tui/       # Ratatui interface
│   ├── henry-telegram/  # Telegram bot (allowlist auth)
│   ├── henry-http/      # Axum REST API + WebSocket
│   ├── henry-server/    # Podman/Docker container mgmt
│   ├── henry-media/     # Jellyfin media server mgmt
│   ├── henry-claude/    # Anthropic API + Claude Code
│   ├── henry-maint/     # Backups, log rotation, updates
│   ├── henry-reactive/  # Anomaly detection, auto-remediation
│   ├── henry-agent/     # ReAct pattern task execution
│   ├── henry-apps/      # Multi-service app deployment from GitHub
│   ├── henry-proxy/     # Caddy reverse proxy management
│   └── henry-tunnel/    # Cloudflare Tunnel management
└── config.example.toml
```

---

## Security

- **Single instance**: Port binding + PID file prevents duplicate daemons
- **Allowlist auth**: Telegram uses user ID/username allowlists
- **Bearer token**: Optional HTTP API authentication
- **Timing-safe comparison**: Tokens compared using constant-time algorithms
- **Secret filtering**: Container inspect hides sensitive environment variables
- **1Password integration**: Secrets never stored in config files
- **Container allowlist**: Restrict which containers can be managed

---

## Development

```bash
# Build debug
cargo build

# Run tests (~153 tests)
cargo test

# Test specific crate
cargo test -p henry-config

# Run with verbose logging
./target/debug/henry --verbose run

# Validate config
./target/debug/henry --config config.example.toml check
```

---

## License

MIT
