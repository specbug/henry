# Deploying Henry to Mac Mini M4

## Quick Deploy

From your development machine:

```bash
# 1. Build for ARM64 (Mac Mini M4)
cargo build --release --target aarch64-apple-darwin

# 2. Copy to Mac Mini
scp target/aarch64-apple-darwin/release/henry user@macmini:~/
scp deploy/setup.sh user@macmini:~/

# 3. SSH to Mac Mini and run setup
ssh user@macmini
chmod +x setup.sh && ./setup.sh
```

## Files in this Directory

| File | Purpose |
|------|---------|
| `setup.sh` | Main setup script - installs binary, creates LaunchAgent |
| `tailscale-setup.sh` | Configures Tailscale Serve for remote HTTPS access |
| `config-template.toml` | Annotated config template with all options |
| `com.henry.daemon.plist` | LaunchAgent template (used by setup.sh) |
| `install.sh` | Alternative installer (runs from project dir) |
| `uninstall.sh` | Removes Henry from system |

## Step-by-Step Setup

### 1. Build the Binary

On your dev machine (cross-compile for ARM64):

```bash
# If you don't have the target installed
rustup target add aarch64-apple-darwin

# Build
cargo build --release --target aarch64-apple-darwin
```

Or build directly on Mac Mini:
```bash
git clone https://github.com/specbug/henry
cd henry
cargo build --release
```

### 2. Transfer to Mac Mini

```bash
# Binary + setup script
scp target/aarch64-apple-darwin/release/henry user@macmini:~/
scp deploy/setup.sh user@macmini:~/
scp deploy/tailscale-setup.sh user@macmini:~/
scp deploy/config-template.toml user@macmini:~/
```

### 3. Run Setup

SSH to Mac Mini:
```bash
ssh user@macmini
chmod +x setup.sh
./setup.sh
```

This will:
- Install henry to `/usr/local/bin/`
- Create `~/.henry/` directory structure
- Generate default config
- Create LaunchAgent for auto-start

### 4. Configure

Edit the config:
```bash
vim ~/.henry/config.toml
```

Key things to configure:
- `[telegram]` - Your bot token and allowed users
- `[containers]` - Socket path if using Colima
- `[media]` - Library paths if using Jellyfin
- `[claude]` - API key reference if using AI features

### 5. Set Up Secrets (1Password)

```bash
# Install 1Password CLI
brew install 1password-cli
op signin

# Store Telegram bot token
op item create --category=login --title="Henry Telegram Bot" credential=YOUR_BOT_TOKEN

# Store Anthropic API key (if using Claude)
op item create --category=login --title="Anthropic API" credential=YOUR_API_KEY
```

### 6. Start the Service

Henry has a built-in install command that manages the LaunchAgent for you:

```bash
# Install as LaunchAgent (auto-starts on login, auto-restarts on crash)
henry install

# Verify
henry status    # Shows "LaunchAgent: installed and running (PID xxx)"
launchctl list | grep henry

# View logs
tail -f ~/.henry/logs/henry.stdout.log
```

Or manually:
```bash
launchctl load ~/Library/LaunchAgents/com.henry.daemon.plist
```

### 7. (Optional) Tailscale Remote Access

Install Tailscale on Mac Mini:
```bash
# Download from https://tailscale.com/download/mac
# Or: brew install tailscale
tailscale login
```

Run the setup script:
```bash
./tailscale-setup.sh
```

Your Henry API will be available at:
```
https://macmini.<your-tailnet>.ts.net/
```

## Service Management

```bash
# Install / Start (preferred)
henry install                 # Installs LaunchAgent, starts immediately
henry install --force         # Reinstall if already present

# Uninstall / Stop
henry uninstall               # Stops and removes LaunchAgent

# Check status
henry status                  # Shows LaunchAgent status + daemon PID

# View logs
tail -f ~/.henry/logs/henry.stdout.log
tail -f ~/.henry/logs/henry.stderr.log

# Manual launchctl (if needed)
launchctl load ~/Library/LaunchAgents/com.henry.daemon.plist
launchctl unload ~/Library/LaunchAgents/com.henry.daemon.plist
```

## Updating Henry

```bash
# Stop service
henry uninstall

# Replace binary
sudo cp ~/henry-new /usr/local/bin/henry

# Start service
henry install
```

## Troubleshooting

### Service won't start
```bash
# Check launchd errors
launchctl error $(launchctl list | grep henry | awk '{print $1}')

# Check stderr
cat ~/.henry/logs/launchd-stderr.log
```

### Can't connect to containers
```bash
# Check socket path
ls -la /var/run/docker.sock
ls -la ~/.colima/default/docker.sock

# Test with podman/docker directly
podman ps
docker ps
```

### 1Password secrets not resolving
```bash
# Make sure you're signed in
op signin

# Test secret access
op read "op://Private/Henry Telegram Bot/credential"
```

### Permission denied
```bash
# Fix binary permissions
sudo chmod 755 /usr/local/bin/henry

# Fix config permissions
chmod 600 ~/.henry/config.toml
```
