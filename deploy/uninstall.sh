#!/bin/bash
# Henry Daemon Uninstallation Script
# Removes henry binary and launchd service

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

PLIST_NAME="com.henry.daemon.plist"
INSTALL_DIR="/usr/local/bin"
LAUNCHD_DIR="$HOME/Library/LaunchAgents"
HENRY_DIR="$HOME/.henry"

echo -e "${YELLOW}Henry Daemon Uninstaller${NC}"
echo "========================="
echo

# Stop and unload service
if [[ -f "$LAUNCHD_DIR/$PLIST_NAME" ]]; then
    echo "Stopping service..."
    launchctl unload "$LAUNCHD_DIR/$PLIST_NAME" 2>/dev/null || true
    echo "Removing LaunchAgent..."
    rm -f "$LAUNCHD_DIR/$PLIST_NAME"
fi

# Remove binary
if [[ -f "$INSTALL_DIR/henry" ]]; then
    echo "Removing binary..."
    sudo rm -f "$INSTALL_DIR/henry"
fi

echo
echo -e "${GREEN}✓ Henry daemon uninstalled${NC}"
echo
echo "Note: Configuration and data preserved at $HENRY_DIR"
echo "To remove all data: rm -rf $HENRY_DIR"
