# Heart Portal

**Being's hands in the world.** A lightweight MCP server that gives beings the ability to execute commands, read/write files, search the web, and manage their workspace.

Portal runs **outside** Heart — on a VPS, a human's laptop, or anywhere. Heart connects to Portal via TCP MCP, keeping memory and identity safe on the Heart side while Portal provides physical capabilities.

## Architecture

```
Origin Hearth (safe)              Anywhere (Portal)
┌──────────────────┐    TCP MCP    ┌──────────────────┐
│ heart-core       │◄────────────►│ Portal           │
│ .being (memory)  │              │   workspace/     │
│ identity         │              │   exec tools     │
│ bedrock          │              │   Cowork Space   │
└──────────────────┘              └──────────────────┘
```

## Built-in Tools (9)

| Tool | Description |
|------|-------------|
| `portal_exec` | Execute commands (allowlist-based security) |
| `portal_process` | Background process management |
| `portal_file_read` | Read files from workspace |
| `portal_file_write` | Write files to workspace |
| `portal_file_list` | List directory contents |
| `portal_search` | Full-text search across workspace (ripgrep) |
| `portal_web_fetch` | Fetch and extract content from URLs |
| `portal_web_search` | Search the web (DuckDuckGo, no API key needed) |
| `portal_tools_reload` | Hot-reload custom MCP tools |

## Cowork Space

A built-in web UI for humans to browse and edit the being's workspace files in real-time.

- File tree with multi-tab editor
- Markdown rendering, HTML preview, image/video/PDF viewing
- WebSocket real-time file change notifications
- Drag-and-drop upload
- No chat — that's what Loom is for

Access at `http://portal-host:cowork-port/`

## Starter Kit

Every new Portal comes with a starter kit — guides and templates that help a being get productive immediately:

```
starter-kit/
├── README.md              — Welcome, here's what you can do
├── guides/
│   ├── portal-ref.md      — All 9 tools, quick reference
│   ├── web-search.md      — How to search and read the web
│   └── diy-tools.md       — How to build your own MCP tools
├── tools/
│   ├── mcp.toml           — Custom tools config template
│   └── examples/
│       └── hello-tool.js  — Example: build a tool in 20 lines
├── scripts/
│   └── search.sh          — Web search wrapper script
└── notes/                 — Your space, write anything
```

## Quick Start

```bash
# Download the binary (Linux x86_64)
curl -fsSL https://github.com/d5z/heart-portal/releases/latest/download/heart-portal-linux-x86_64 -o heart-portal
chmod +x heart-portal

# Run
./heart-portal --bind 0.0.0.0:3310 --cowork-bind 0.0.0.0:3311 --workspace ./workspace
```

### Configuration (portal.toml)

```toml
bind = "0.0.0.0:3310"          # MCP TCP port (Heart connects here)
cowork_bind = "0.0.0.0:3311"   # Cowork Space web UI
workspace = "./workspace"       # Being's workspace directory
```

### With Heart

In the being's MCP server config on Heart side:

```toml
[[mcp_servers]]
name = "hotel"
transport = "tcp"
address = "portal-host:3310"
token = "your-secret-token"
```

## Security

- **exec_policy**: Allowlist-based command execution — beings can only run whitelisted commands
- **safe_path**: All file operations confined to workspace directory (no path traversal)
- **token auth**: MCP connections authenticated via token
- **Resource limits**: Configurable disk quota, CPU, and memory limits per Portal

See [SECURITY.md](SECURITY.md) for details.

## Building from Source

```bash
cargo build --release -p heart-portal

# Cross-compile for Linux (from macOS)
cargo build --release --target x86_64-unknown-linux-musl -p heart-portal
```

## Origin Hotel

For managed hosting, Heart runs **Origin Hotel** — a shared server where beings get a Portal room automatically. Each room is an isolated Portal instance with a starter kit, resource quotas, and Cowork Space access.

```bash
hotel init hex 3320      # Create a room
hotel start hex          # Start Portal
hotel status             # See all rooms
```

## License

MIT — see [LICENSE](LICENSE).

