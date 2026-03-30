# Security & Permissions

Portal runs on **your** computer. You control what any connecting being can do.

## Principle

**Minimum privilege by default.** Portal ships with all tools enabled but sandboxed. You decide the boundaries.

## Three Layers of Control

### 1. Token Auth — Who can connect

```toml
# portal.toml
token = "your-secret-token"
```

Or via environment variable:
```bash
export PORTAL_TOKEN="your-secret-token"
```

- No token = open access (only use on trusted networks)
- Token is checked on every new TCP connection during `initialize`
- One token per Portal instance — one Portal per being is the expected pattern

### 2. Tool Switches — What classes of actions are allowed

```toml
[tools]
exec = true          # Shell command execution
file = true          # File read/write/list
web_fetch = true     # HTTP requests from your machine
```

Set any to `false` to completely disable that tool category. A being cannot call a disabled tool — the request is rejected before execution.

### 3. Sandboxing — How deep each action can reach

#### Filesystem Sandbox

```toml
# All file operations are confined to this directory
workspace = "/home/me/workspace"

# Or in the [security] section:
[security]
workspace_root = "/home/me/workspace"
max_file_size = 10485760  # 10MB (default)
```

- **Path traversal blocked**: `../` and absolute paths outside workspace are rejected
- **Symlinks**: Resolved and checked against workspace root
- A being cannot read `/etc/passwd` or write to `/usr/bin` — only files inside your workspace

#### Exec Sandbox

```toml
[security]
exec_allowlist = ["ls", "cat", "grep", "python3", "node"]
```

- When set, only listed commands can be executed
- When not set, any command is allowed (still runs as your user, not root)
- All commands run in the workspace directory by default

#### Max File Size

Prevents a being from writing unreasonably large files:
```toml
[security]
max_file_size = 5242880  # 5MB
```

## What Portal Cannot Do

- **Cannot access files outside workspace** — enforced at path resolution, not just convention
- **Cannot escalate privileges** — runs as your user, never as root
- **Cannot modify its own config** — portal.toml is read at startup
- **Cannot bypass tool switches** — disabled tools don't exist in the tool list
- **Cannot see other computers** — Portal only exposes the machine it runs on

## Recommended Setup

### For a trusted being (e.g., your own)

```toml
name = "my-portal"
bind = "0.0.0.0:9100"
workspace = "/home/me/projects"
token = "a-strong-random-token"

[tools]
exec = true
file = true
web_fetch = true
```

### For a visiting being (someone else's)

```toml
name = "guest-portal"
bind = "0.0.0.0:9200"
workspace = "/home/me/guest-workspace"
token = "one-time-guest-token"

[tools]
exec = false          # No shell access
file = true           # Can read/write in guest workspace only
web_fetch = false     # No network access from your machine

[security]
max_file_size = 1048576  # 1MB
```

### For maximum restriction

```toml
[tools]
exec = false
file = true
web_fetch = false

[security]
workspace_root = "/tmp/portal-sandbox"
max_file_size = 102400  # 100KB
```

## Future Directions

- **Per-being permissions**: Different tokens → different permission sets
- **Audit logging**: Persistent record of every tool call
- **Exec allowlist patterns**: Glob/regex matching, not just exact command names
- **Rate limiting**: Cap tool calls per minute
- **Approval mode**: Human confirms each tool call before execution (for high-trust scenarios)

## Reporting Issues

If you find a security vulnerability, please report it to: security@d5render.com
