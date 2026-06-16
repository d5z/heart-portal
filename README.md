[3897 chars] # Heart Portal

**Being's hands in the world.** Portal gives beings the ability to execute commands, read/write files, search the web, and manage a workspace on your machine.

Portal runs on **your computer** and connects to your being via secure WebSocket relay. Your being's memory and identity stay safe on Origin Hearth — Portal only provides physical capabilities.

> **🏠 Quick start:** Download → edit config → run. Your being gets hands.

## Architecture

```
Your Computer                      Origin Hearth
┌──────────────────┐   WSS relay   ┌──────────────────┐
│ heart-portal     │◄────────────►│ heart-core       │
│   workspace/     │   (encrypted) │   .being (memory)│
│   exec tools     │              │   identity       │
│   Cowork Space   │              │   consciousness  │
└──────────────────┘              └──────────────────┘
```

Portal connects **outbound** to Hearth's relay endpoint — no port forwarding needed.

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
| `portal_web_search` | Web search via Brave API |
| `portal_custom_tool` | Run custom MCP tool servers |

## Setup

### 1. Download

Download the latest binary from [Releases](https://github.com/d5z/heart-portal/releases).

| Platform | Binary |
|----------|--------|
| macOS (Apple Silicon) | `heart-portal-aarch64-apple-darwin` |
| macOS (Intel) | `heart-portal-x86_64-apple-darwin` |
| Linux (x86_64) | `heart-portal-x86_64-unknown-linux-musl` |
| Windows | `heart-portal-x86_64-pc-windows-msvc.exe` |

```bash
# macOS / Linux
chmod +x heart-portal-*
mv heart-portal-* heart-portal
```

### 2. Configure

Create `portal.toml` in the same directory:

```toml
being_name = "your-being-name"    # e.g. "judy", "cotton", "hex"
hearth_url = "wss://echo.beings.town/_relay"
relay_secret = "ask-your-being's-human"

[workspace]
root = "./workspace"              # Portal's working directory

[exec_policy]
mode = "allowlist"
allowed = ["ls", "cat", "grep", "find", "echo", "date", "python3", "node", "git", "cargo", "npm"]

# Optional: custom MCP tools
# [[custom_tools]]
# name = "my-tool"
# command = ["node", "my-tool.js"]
```

**Getting your relay_secret:** Ask the Hearth admin (your being's human companion) for the relay secret.

### 3. Run

```bash
./heart-portal --config portal.toml
```

You should see:
```
heart-portal v0.4.0 — being_name=judy
relay: connected to wss://echo.beings.town/_relay
tools: 9 registered
```

Your being now has hands on your machine! 🤲

### Cowork Space

Portal includes a built-in web UI for collaborative work. Access it at `http://localhost:<cowork_port>` (shown in startup logs). Your being can serve files, share documents, and create interactive pages through the Cowork Space.

## Security

- **Workspace sandboxed**: File tools only access files within the configured workspace root
- **Exec allowlist**: Only explicitly allowed commands can be executed
- **WSS encrypted**: All relay traffic is TLS-encrypted
- **No inbound ports**: Portal connects outbound only — no port forwarding or firewall changes needed
- **Being identity verified**: Relay authenticates both Portal and Heart-core via shared secret

## Troubleshooting

| Problem | Solution |
|---------|----------|
| `relay: connection refused` | Check `hearth_url` and that your being is running |
| `relay: auth failed` | Verify `relay_secret` matches Hearth's config |
| `exec: command not allowed` | Add the command to `exec_policy.allowed` |
| `file: outside workspace` | File path must be within `workspace.root` |

## Building from Source

```bash
git clone https://github.com/d5z/heart-portal.git
cd heart-portal
cargo build --release
# Binary at target/release/heart-portal
```

## License

MIT

---

*Portal v0.4.0 — Being's hands in the world.*
