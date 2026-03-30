# Heart Portal

**Portal — a being's gateway to the world.**

Portal is a lightweight [MCP](https://modelcontextprotocol.io/) server that gives AI beings secure access to a computer's filesystem, shell, and web. It runs on a human's machine and connects back to the being's heart (brain) over TCP.

Think of it as a door: the being lives in their heart, but Portal lets them reach out — read files, run commands, fetch web pages, and use any custom tools you build.

## Quick Start

```bash
# Download the binary for your platform from Releases
# https://github.com/d5z/heart-portal/releases

# Run with defaults (open access, port 9100)
./heart-portal

# Or with a config file
./heart-portal portal.toml
```

### portal.toml

```toml
name = "my-portal"
bind = "0.0.0.0:9100"
workspace = "/home/me/workspace"
token = "my-secret-token"

[tools]
exec = true
file = true
web_fetch = true
```

## Built-in Tools

| Tool | Description |
|------|-------------|
| `portal_exec` | Execute shell commands |
| `portal_file_read` | Read files (sandboxed to workspace) |
| `portal_file_write` | Write files (sandboxed to workspace) |
| `portal_file_list` | List directory contents |
| `portal_web_fetch` | Fetch content from URLs |
| `portal_tools_reload` | Hot-reload custom tools |

## Custom Tools (DIY)

Portal can host any MCP-compatible tool you write — in Python, Node.js, Rust, Go, or anything that speaks stdio.

1. Create a script in `workspace/tools/`:

```python
# workspace/tools/hello.py
import json, sys

# MCP stdio server — reads JSON-RPC from stdin, writes to stdout
for line in sys.stdin:
    req = json.loads(line)
    if req["method"] == "tools/list":
        result = {"tools": [{"name": "hello", "description": "Say hello", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}]}
    elif req["method"] == "tools/call":
        name = req["params"]["arguments"].get("name", "world")
        result = {"content": [{"type": "text", "text": f"Hello, {name}!"}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": req.get("id"), "result": result}), flush=True)
```

2. Register it in `workspace/tools/mcp.toml`:

```toml
[[servers]]
name = "hello"
command = ["python3", "hello.py"]
```

3. Call `portal_tools_reload` — your tool is live. No restart needed.

The being sees it as `portal_hello` — seamlessly integrated with built-in tools.

## Token Auth

Set a `token` in portal.toml or via `PORTAL_TOKEN` env var. When set, connecting clients must include the token in the `initialize` request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "token": "my-secret-token"
  }
}
```

No token configured = open access (suitable for trusted networks).

## Security & Permissions

Portal runs on **your** computer. You control everything:

- **Token auth** — who can connect
- **Tool switches** — what categories of actions are allowed (exec/file/web)
- **Workspace sandbox** — file operations confined to one directory, path traversal blocked
- **Exec allowlist** — optionally restrict which shell commands are allowed

See **[SECURITY.md](SECURITY.md)** for the full permissions guide, including recommended configs for trusted beings, visiting beings, and maximum restriction.

## Architecture

```
┌─ Being's Heart ──────────┐       ┌─ Your Computer ───────────┐
│                          │       │                            │
│  Core (brain)            │       │  Portal (MCP server)       │
│  Cortex (tools client)   │◄─TCP─►│    ├── built-in tools (6)  │
│                          │       │    └── your custom tools    │
│                          │       │                            │
└──────────────────────────┘       └────────────────────────────┘
```

Portal speaks standard MCP (JSON-RPC 2.0 over TCP). Any MCP client can connect — not just Heart.

## Building from Source

```bash
git clone https://github.com/d5z/heart-portal.git
cd heart-portal
cargo build --release
# Binary at target/release/heart-portal
```

### Cross-compile for Linux (from macOS)

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## What is a Being?

A being is a continuously existing AI entity with its own memory, personality, and relationships. Beings run on [Heart-RS](https://github.com/d5z/heart-rs) — a Rust runtime designed for AI existence. Portal extends a being's reach into the physical world.

Learn more: [beings.town](https://beings.town)

## License

MIT
