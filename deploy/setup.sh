#!/bin/bash
# Henry Daemon - Complete Setup Script for Mac Mini M4
# Run this on your Mac Mini after transferring the henry binary
#
# Usage:
#   1. Build on dev machine: cargo build --release
#   2. Copy to Mac Mini: scp target/release/henry user@macmini:~/
#   3. Copy this script: scp deploy/setup.sh user@macmini:~/
#   4. Run on Mac Mini: chmod +x setup.sh && ./setup.sh

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}"
echo "╔════════════════════════════════════════╗"
echo "║     Henry Daemon Setup - Mac Mini      ║"
echo "╚════════════════════════════════════════╝"
echo -e "${NC}"

# Paths
HENRY_BIN="$HOME/henry"
INSTALL_DIR="/usr/local/bin"
HENRY_DIR="$HOME/.henry"
LOGS_DIR="$HENRY_DIR/logs"
STATE_DIR="$HENRY_DIR/state"
BACKUP_DIR="$HENRY_DIR/backups"
LAUNCHD_DIR="$HOME/Library/LaunchAgents"
PLIST_NAME="com.henry.daemon.plist"

# Check binary exists
if [[ ! -f "$HENRY_BIN" ]]; then
    echo -e "${RED}Error: henry binary not found at $HENRY_BIN${NC}"
    echo "Please copy the binary to your home directory first:"
    echo "  scp target/release/henry user@macmini:~/"
    exit 1
fi

# Check architecture
ARCH=$(file "$HENRY_BIN" | grep -o "arm64\|x86_64" | head -1)
if [[ "$ARCH" != "arm64" ]]; then
    echo -e "${YELLOW}Warning: Binary is $ARCH, Mac Mini M4 is arm64${NC}"
    echo "Consider rebuilding with: cargo build --release --target aarch64-apple-darwin"
fi

echo -e "${GREEN}Step 1: Creating directories...${NC}"
mkdir -p "$HENRY_DIR"
mkdir -p "$LOGS_DIR"
mkdir -p "$STATE_DIR"
mkdir -p "$BACKUP_DIR"
mkdir -p "$HENRY_DIR/cache"
mkdir -p "$LAUNCHD_DIR"
echo "  ✓ Created $HENRY_DIR"

echo -e "${GREEN}Step 2: Installing binary...${NC}"
sudo cp "$HENRY_BIN" "$INSTALL_DIR/henry"
sudo chmod 755 "$INSTALL_DIR/henry"
echo "  ✓ Installed to $INSTALL_DIR/henry"

echo -e "${GREEN}Step 3: Generating configuration...${NC}"
if [[ ! -f "$HENRY_DIR/config.toml" ]]; then
    /usr/local/bin/henry init
    echo "  ✓ Generated default config at $HENRY_DIR/config.toml"
else
    echo "  ✓ Config already exists at $HENRY_DIR/config.toml"
fi

echo -e "${GREEN}Step 4: Creating LaunchAgent...${NC}"
cat > "$LAUNCHD_DIR/$PLIST_NAME" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.henry.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/henry</string>
        <string>run</string>
        <string>--headless</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
    </dict>
    <key>StandardOutPath</key>
    <string>__LOGS__/launchd-stdout.log</string>
    <key>StandardErrorPath</key>
    <string>__LOGS__/launchd-stderr.log</string>
    <key>SoftResourceLimits</key>
    <dict>
        <key>NumberOfFiles</key>
        <integer>4096</integer>
    </dict>
</dict>
</plist>
PLIST

# Substitute paths
sed -i '' "s|__LOGS__|$LOGS_DIR|g" "$LAUNCHD_DIR/$PLIST_NAME"
chmod 644 "$LAUNCHD_DIR/$PLIST_NAME"
echo "  ✓ Created LaunchAgent at $LAUNCHD_DIR/$PLIST_NAME"

echo -e "${GREEN}Step 5: Checking prerequisites...${NC}"

# Check 1Password CLI
if command -v op &> /dev/null; then
    echo "  ✓ 1Password CLI installed"
else
    echo -e "  ${YELLOW}⚠ 1Password CLI not found${NC}"
    echo "    Install with: brew install 1password-cli"
fi

# Check Podman/Docker
if command -v podman &> /dev/null; then
    echo "  ✓ Podman installed"
elif command -v docker &> /dev/null; then
    echo "  ✓ Docker installed"
else
    echo -e "  ${YELLOW}⚠ No container runtime found${NC}"
    echo "    Install with: brew install podman"
fi

# Check Tailscale
if command -v tailscale &> /dev/null; then
    echo "  ✓ Tailscale installed"
else
    echo -e "  ${YELLOW}⚠ Tailscale not found (optional for remote access)${NC}"
    echo "    Install from: https://tailscale.com/download/mac"
fi

echo -e "${GREEN}Step 6: Validating configuration...${NC}"
if /usr/local/bin/henry check 2>/dev/null; then
    echo "  ✓ Configuration valid"
else
    echo -e "  ${YELLOW}⚠ Configuration needs editing${NC}"
    echo "    Edit: $HENRY_DIR/config.toml"
fi

echo
echo -e "${BLUE}Setup complete!${NC}"
echo
echo "Next steps:"
echo "  1. Edit configuration:"
echo "     vim $HENRY_DIR/config.toml"
echo
echo "  2. Configure secrets in 1Password (if using Telegram/Claude):"
echo "     op item create --category=login --title=\"Henry Telegram Bot\" credential=YOUR_TOKEN"
echo
echo "  3. Start the service:"
echo "     launchctl load $LAUNCHD_DIR/$PLIST_NAME"
echo
echo "  4. Check status:"
echo "     henry status"
echo "     launchctl list | grep henry"
echo
echo "  5. View logs:"
echo "     tail -f $LOGS_DIR/launchd-stdout.log"
echo
echo "To stop the service:"
echo "  launchctl unload $LAUNCHD_DIR/$PLIST_NAME"
