# Henry

A security-first, minimal Rust daemon for Mac Mini M4 Pro that manages home server functions.

## Features

- **Container Management**: Deploy and manage containers via Podman
- **Communication**: Telegram bot with allowlist authentication
- **AI Integration**: Spawn Claude Code sessions
- **Media Server**: Jellyfin with hardware transcoding (VideoToolbox)
- **Self-Maintenance**: Autonomous backups, updates, and health monitoring
- **TUI**: Claude Code-inspired terminal interface

## Quick Start

```bash
# Build
cargo build --release

# Generate config
./target/release/henry init

# Edit config.toml with your settings

# Validate config
./target/release/henry check

# Run with TUI
./target/release/henry run

# Run headless (for systemd/launchd)
./target/release/henry run --headless
```

## Configuration

Copy `config.example.toml` to `~/.henry/config.toml` and configure:

1. **Telegram Bot**: Get a token from @BotFather, store in 1Password
2. **Allowed Users**: Add your Telegram user ID to `allowed_users`
3. **Containers**: Enable Podman support
4. **Media**: Configure library paths for Jellyfin

### Secret Management

Henry uses 1Password CLI for secrets:

```bash
# Install 1Password CLI
brew install 1password-cli

# Sign in
op signin

# Store secrets
op item create --category=login --title="Henry Telegram Bot" credential=YOUR_BOT_TOKEN

# Reference in config
token_ref = "op://Private/Henry Telegram Bot/credential"
```

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
│   ├── henry-http/      # Axum REST API
│   ├── henry-server/    # Podman/Docker container mgmt
│   └── henry-media/     # Jellyfin media server mgmt
└── config.example.toml
```

## Keyboard Shortcuts (TUI)

| Key | Action |
|-----|--------|
| `Tab` | Switch views |
| `1-4` | Jump to view |
| `F1` | Help |
| `F5` | Refresh metrics |
| `:` | Command mode |
| `q` | Quit |

## Signals

- `SIGTERM` / `SIGINT`: Graceful shutdown
- `SIGUSR1` / `SIGHUP`: Reload configuration

## License

MIT
