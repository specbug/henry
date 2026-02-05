#!/bin/bash
# Henry Daemon Installation Script
# Installs henry binary and sets up launchd service on macOS

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
PLIST_NAME="com.henry.daemon.plist"
INSTALL_DIR="/usr/local/bin"
LAUNCHD_DIR="$HOME/Library/LaunchAgents"
HENRY_DIR="$HOME/.henry"
LOGS_DIR="$HENRY_DIR/logs"

echo -e "${GREEN}Henry Daemon Installer${NC}"
echo "======================="
echo

# Check if running on macOS
if [[ "$(uname)" != "Darwin" ]]; then
    echo -e "${RED}Error: This script is for macOS only${NC}"
    exit 1
fi

# Check if binary exists
BINARY="$PROJECT_DIR/target/release/henry"
if [[ ! -f "$BINARY" ]]; then
    echo -e "${YELLOW}Release binary not found. Building...${NC}"
    cd "$PROJECT_DIR"
    cargo build --release
    if [[ ! -f "$BINARY" ]]; then
        echo -e "${RED}Error: Failed to build henry${NC}"
        exit 1
    fi
fi

# Stop existing service if running
if launchctl list 2>/dev/null | grep -q "com.henry.daemon"; then
    echo -e "${YELLOW}Stopping existing service...${NC}"
    launchctl unload "$LAUNCHD_DIR/$PLIST_NAME" 2>/dev/null || true
fi

# Create directories
echo "Creating directories..."
mkdir -p "$HENRY_DIR"
mkdir -p "$LOGS_DIR"
mkdir -p "$HENRY_DIR/state"
mkdir -p "$HENRY_DIR/backups"
mkdir -p "$HENRY_DIR/cache"
mkdir -p "$LAUNCHD_DIR"

# Install binary
echo "Installing binary to $INSTALL_DIR..."
sudo cp "$BINARY" "$INSTALL_DIR/henry"
sudo chmod 755 "$INSTALL_DIR/henry"

# Create config if doesn't exist
if [[ ! -f "$HENRY_DIR/config.toml" ]]; then
    echo "Generating default configuration..."
    "$INSTALL_DIR/henry" init
fi

# Install plist with HOME substitution
echo "Installing LaunchAgent..."
sed "s|__HOME__|$HOME|g" "$SCRIPT_DIR/$PLIST_NAME" > "$LAUNCHD_DIR/$PLIST_NAME"
chmod 644 "$LAUNCHD_DIR/$PLIST_NAME"

# Validate configuration
echo "Validating configuration..."
if ! "$INSTALL_DIR/henry" check; then
    echo -e "${YELLOW}Warning: Configuration validation failed. Please edit $HENRY_DIR/config.toml${NC}"
fi

# Load service
echo "Loading service..."
launchctl load "$LAUNCHD_DIR/$PLIST_NAME"

# Check if running
sleep 2
if launchctl list 2>/dev/null | grep -q "com.henry.daemon"; then
    echo -e "${GREEN}✓ Henry daemon installed and running${NC}"
else
    echo -e "${YELLOW}Warning: Service may not have started. Check logs at $LOGS_DIR${NC}"
fi

echo
echo "Installation complete!"
echo
echo "Useful commands:"
echo "  henry status              - Check daemon status"
echo "  launchctl list | grep henry  - Check launchd status"
echo "  tail -f ~/.henry/logs/launchd-stdout.log  - View stdout"
echo "  tail -f ~/.henry/logs/launchd-stderr.log  - View stderr"
echo
echo "To uninstall:"
echo "  $SCRIPT_DIR/uninstall.sh"
