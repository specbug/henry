#!/bin/bash
# Henry Daemon - Tailscale Serve Setup
# Exposes Henry's HTTP API over Tailscale with HTTPS
#
# Prerequisites:
#   - Tailscale installed and logged in
#   - Henry daemon running
#
# This creates a secure HTTPS endpoint at:
#   https://henry.<your-tailnet>.ts.net

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

HENRY_PORT="${HENRY_PORT:-18791}"
TAILSCALE_HOSTNAME="${TAILSCALE_HOSTNAME:-henry}"

echo -e "${BLUE}Tailscale Serve Setup for Henry${NC}"
echo "================================="
echo

# Check Tailscale
if ! command -v tailscale &> /dev/null; then
    echo -e "${RED}Error: Tailscale not installed${NC}"
    echo "Install from: https://tailscale.com/download/mac"
    exit 1
fi

# Check Tailscale status
if ! tailscale status &> /dev/null; then
    echo -e "${RED}Error: Tailscale not running or not logged in${NC}"
    echo "Run: tailscale login"
    exit 1
fi

# Get current Tailscale hostname
CURRENT_HOSTNAME=$(tailscale status --json | grep -o '"Self":{[^}]*"HostName":"[^"]*"' | grep -o '"HostName":"[^"]*"' | cut -d'"' -f4)
echo "Current Tailscale hostname: $CURRENT_HOSTNAME"

# Check if Henry is running
if ! curl -s "http://localhost:$HENRY_PORT/health" > /dev/null 2>&1; then
    echo -e "${YELLOW}Warning: Henry doesn't appear to be running on port $HENRY_PORT${NC}"
    echo "Start Henry first, or set HENRY_PORT environment variable"
fi

echo
echo "This will expose Henry's HTTP API at:"
echo "  https://$CURRENT_HOSTNAME.<tailnet>.ts.net"
echo
read -p "Continue? (y/n) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Aborted."
    exit 0
fi

echo -e "${GREEN}Configuring Tailscale Serve...${NC}"

# Reset any existing serve config for this port
tailscale serve reset 2>/dev/null || true

# Set up HTTPS serve pointing to Henry's HTTP API
tailscale serve https / http://localhost:$HENRY_PORT

echo
echo -e "${GREEN}✓ Tailscale Serve configured!${NC}"
echo
echo "Your Henry API is now available at:"
TAILNET=$(tailscale status --json | grep -o '"MagicDNSSuffix":"[^"]*"' | cut -d'"' -f4)
echo "  https://$CURRENT_HOSTNAME.$TAILNET"
echo
echo "Test it:"
echo "  curl https://$CURRENT_HOSTNAME.$TAILNET/health"
echo
echo "WebSocket:"
echo "  websocat wss://$CURRENT_HOSTNAME.$TAILNET/ws"
echo
echo "To view serve status:"
echo "  tailscale serve status"
echo
echo "To disable:"
echo "  tailscale serve reset"
