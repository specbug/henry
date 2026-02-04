# Phase 5: AI Integration - Implementation Notes

**Completed:** 2026-02-05
**Commits:** `218c461af`, `03c1bb7cd`
**Branch:** `init`

## Overview

Added `henry-claude` crate for Anthropic API access and Claude Code session management.

## Files Created

```
crates/henry-claude/
├── Cargo.toml
└── src/
    ├── lib.rs          # ClaudeManager, ClaudeError, format_session()
    ├── api.rs          # AnthropicClient - Messages API HTTP client
    ├── session.rs      # SessionManager - Claude Code process spawning
    └── types.rs        # SessionInfo, SessionStatus, ClaudeStatus, Message, MessageResponse
```

## Files Modified

- `Cargo.toml` - Added henry-claude to workspace, added uuid/which/serde_json deps
- `crates/henry-core/Cargo.toml` - Added henry-claude dependency
- `crates/henry-core/src/daemon.rs` - Initialize ClaudeManager, pass to services
- `crates/henry-telegram/Cargo.toml` - Added henry-claude dependency
- `crates/henry-telegram/src/lib.rs` - Added /claude command + handlers
- `crates/henry-http/Cargo.toml` - Added henry-claude dependency
- `crates/henry-http/src/lib.rs` - Added /api/claude/* endpoints
- `PLAN.md` - Updated with Phase 5 details

## Key Types

```rust
// Session lifecycle
pub enum SessionStatus { Starting, Running, Stopping, Stopped, Error }

// Session tracking
pub struct SessionInfo {
    pub id: String,
    pub workspace: String,
    pub status: SessionStatus,
    pub pid: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub error: Option<String>,
}

// Module status
pub struct ClaudeStatus {
    pub enabled: bool,
    pub api_key_configured: bool,
    pub active_sessions: usize,
    pub max_sessions: usize,
    pub workspace_dir: String,
    pub sessions: Vec<SessionInfo>,
}

// API types
pub struct Message { pub role: MessageRole, pub content: String }
pub struct MessageResponse { pub id, model, content, input_tokens, output_tokens, stop_reason }
```

## Service Signatures (Updated)

All services now accept `claude: Option<Arc<RwLock<ClaudeManager>>>`:

```rust
// HTTP
henry_http::run_server(config, &bind_addr, health, api_token, containers, media, claude, shutdown_rx)

// Telegram
henry_telegram::run_bot(token, config, health, containers, media, claude, shutdown_rx)
```

## Config

```toml
[claude]
enabled = true
api_key_ref = "op://Private/Anthropic API/credential"
max_sessions = 4
workspace_dir = "~/claude-workspaces"
```

## Telegram Commands

| Command | Description |
|---------|-------------|
| `/claude` | Show status |
| `/claude status` | Show status |
| `/claude list` | List all sessions |
| `/claude new <workspace>` | Start new session |
| `/claude stop <id>` | Stop session |
| `/claude ask <question>` | Query Claude API |

## HTTP API Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/claude/status` | Module status |
| GET | `/api/claude/sessions` | List sessions |
| POST | `/api/claude/sessions/new` | Start session `{"workspace": "name"}` |
| GET | `/api/claude/sessions/{id}` | Get session |
| POST | `/api/claude/sessions/{id}/stop` | Stop session |
| POST | `/api/claude/ask` | Query API `{"question": "...", "model": "...", "max_tokens": N}` |

## Tests Added

21 new tests in henry-claude:
- `types::tests` - 4 tests (status, lifecycle, error, messages)
- `api::tests` - 4 tests (client creation, URL, serialization)
- `session::tests` - 6 tests (manager, workspace, operations)
- `lib::tests` - 7 tests (config, status, formatting)

**Total tests:** 72

## Binary Size

12MB (release, LTO, stripped) - up from 10MB in Phase 4

## Dependencies Added

- `uuid = { version = "1", features = ["v4"] }` - Session ID generation
- `which = "7"` - Find claude binary in PATH
- `serde_json = "1"` - API request/response serialization

## Claude Binary Discovery

`SessionManager::find_claude_binary()` checks these locations:
1. `~/.local/bin/claude`
2. `~/Library/pnpm/claude`
3. `/opt/homebrew/bin/claude`
4. `/usr/local/bin/claude`
5. `/usr/bin/claude`
6. Falls back to `which::which("claude")`

## Next: Phase 6 - Self-Maintenance

From PLAN.md:
- `henry-maint` crate
- Cron scheduler (`tokio-cron-scheduler`)
- Automated backups
- Log rotation
- Self-update checks
