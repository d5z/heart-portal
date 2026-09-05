# Heart Portal

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

## Built-in Tools

| Tool | Parameters | Description |
|------|------------|-------------|
| `portal_exec` | `command`, `workdir`, `timeout_secs`, `background` | Execute shell commands with allowlist-based security. |
| `portal_process` | `action`, `session_id`, `data` | Manage background command sessions. |
| `portal_file_read` | `path`, `offset`, `limit` | Read files from the workspace. |
| `portal_file_write` | `path`, `content` | Write files inside the workspace. |
| `portal_file_edit` | `path`, edit parameters | Edit a file inside the workspace. |
| `portal_file_list` | `path` | List directory contents. |
| `portal_web_fetch` | `url` | Fetch content from a URL. |
| `portal_web_search` | `query` | Search the web. |
| `portal_search` | `query` | Search text across the workspace. |
| `portal_screenshot` | `path`, `region`, `display` | Capture a screenshot to a workspace file. |
| `portal_oauth_authorize` | `provider`, `timeout_secs` | Start an OAuth authorization flow. |
| `portal_tools_reload` | none | Reload custom tools from `workspace/tools/mcp.toml`. |
| `portal_restart` | none | Restart a supervised Portal after returning the tool response. |
| `portal_kit_usage` | none | Drain successful kit call counters. |

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

Copy [portal.example.toml](portal.example.toml) from the source checkout (or
download it next to the release binary), then edit the local file:

```bash
# macOS / Linux
cp portal.example.toml portal.toml
```

```powershell
# Windows
Copy-Item portal.example.toml portal.toml
```

`portal.toml` is intentionally Git-ignored because it contains machine-local
paths and may contain local authentication settings. Commit changes to
`portal.example.toml`, never a user's `portal.toml`.

### 3. Run

```bash
PORTAL_CONNECT_LINK="https://echo.beings.town/<being>/?token=<token>" \
  ./heart-portal --config portal.toml --name "<stable-machine-name>"
```

`--connect <url>` is also supported for interactive use. A supervisor should
prefer `PORTAL_CONNECT_LINK` so the credential is not shown in the process
command line.

### Windows background recovery

For a Windows machine that must recover without local access, install the
supervisor with that machine's own Loom link and a unique Portal name:

```powershell
.\scripts\install-portal-windows.ps1 `
  -ConnectLink "https://echo.beings.town/<being>/?token=<token>"
```

From a source checkout, the installer builds the Release binary when it is
missing and creates `portal.toml` from `portal.example.toml`. To use rsproxy
for that build in China, add `-UseRsproxy`.

By default the installer derives a unique, stable name from the Being and the
Windows computer name. Pass `-PortalName` only when an explicit name is needed.

The installer stores the link in the ignored `.portal-connection.url` file.
It also persists the machine's stable relay identity in the ignored
`.portal-name` file and the scheduled-task name in `.portal-task-name`, so
manual maintenance cannot silently target a different identity or task.
The scheduled task uses a windowless launcher, starts the Release binary at
logon, and restarts it after any exit. No PowerShell or Terminal window should
remain visible.

After updating a kit, the Being should call the built-in `portal_restart` tool.
Portal acknowledges the request, closes cleanly, and the existing supervisor
relaunches it after five seconds with the same persisted name. The tool is only
advertised when Portal was launched by a supervisor. Do not restart Portal
with `portal_exec`, `taskkill`, or by launching a second supervisor.

To remove the supervisor:

```powershell
.\scripts\uninstall-portal-task.ps1
```

Use a unique `--name` per machine. The Portal binary also enforces a
per-relay Windows single-instance lock, so stale duplicate processes cannot
eject the active connection.

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
| `relay: connection refused` | Check the Loom URL and network; Portal retries automatically. |
| `relay: auth failed` | Obtain a fresh Loom link and rerun the Windows installer. |
| `exec: command not allowed` | Add the executable to `security.exec_allowlist`, or leave it empty for unrestricted exec. |
| `file: outside workspace` | File paths must stay inside the configured `workspace`. |
| `portal_restart` is unavailable | Install/run Portal under an external supervisor first. |

## Building from Source

```bash
git clone https://github.com/d5z/heart-portal.git
cd heart-portal
cargo build --release --locked
# Binary at target/release/heart-portal
```

Optional rsproxy build:

```powershell
cargo --config .cargo/rsproxy.toml build --release --locked
```

## License

MIT

---

*Portal v0.8.0 — Being's hands in the world.*
