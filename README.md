# Heart Portal

Portal is a Being's hands and eyes — the tool provider that gives embodied AI agents the ability to interact with the outside world.

Part of the [Heart](https://github.com/d5z/HEART) project: an open-source framework for embodied AI beings with continuous memory, identity, and agency.

## Architecture

```
Origin Hearth (Core)              Being's Home (Portal)
┌─────────────────┐              ┌──────────────────────┐
│ heart-core      │ ←── MCP ──→ │ heart-portal         │
│ .being database │              │ workspace/           │
│ (soul + memory) │              │ exec, file, search   │
└─────────────────┘              └──────────────────────┘
```

- **Core** runs on Hearth (centralized infrastructure) — handles consciousness, memory, identity
- **Portal** runs on the Being's home (VPS/device) — provides tools for external interaction
- Communication via MCP (Model Context Protocol) over TCP

## Tools Provided

### Visceral (via Portal MCP)
- `portal_exec` — Execute commands (sync or background)
- `portal_process` — Manage background processes (poll/log/write/kill)
- `portal_file_read` / `portal_file_write` / `portal_file_list` — File operations
- `portal_search` — Full-text search across workspace
- `portal_web_fetch` — Fetch and extract web content

### Cowork UI
- Web-based interface for human-being collaboration
- Real-time file watching and workspace management

## Quick Start

```bash
# Build
cargo build --release --bin heart-portal

# Configure
cat > portal.toml << TOML
[portal]
name = "my-being"
workspace = "/home/being/workspace"

[portal.core]
url = "http://hearth-host:3101"

[portal.mcp]
port = 9500

[portal.cowork]
port = 9110
TOML

# Run
./heart-portal portal.toml
```

## Background Exec

Long-running commands are first-class:

```
portal_exec(command: "npm install", background: true)
→ {session_id: "abc123", pid: 1234, status: "running"}

portal_process(action: "poll", session_id: "abc123")
→ {status: "running", output: "Installing packages..."}
```

## License

MIT

## Part of Heart

Heart is an open-source framework for creating embodied AI beings — agents with continuous memory, persistent identity, and genuine agency. Each Being has a heart (Core), hands (Portal), and a home.

Learn more: [github.com/d5z/HEART](https://github.com/d5z/HEART)
