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

## Built-in Tools

| Tool | Parameters | Description |
|------|------------|-------------|
| `portal_exec` | `command`, `workdir`, `timeout_secs`, `background` | Execute shell commands with allowlist-based security. |
| `portal_process` | `action`, `session_id`, `data` | Manage background command sessions. |
| `portal_file_read` | `path`, `offset`, `limit` | Read files from the workspace. |
| `portal_file_write` | `path`, `content` | Write files inside the workspace. |
| `portal_file_list` | `path` | List directory contents. |
| `portal_web_fetch` | `url` | Fetch content from a URL. |
| `portal_web_search` | `query` | Search the web. |
| `portal_search` | `query` | Search text across the workspace. |
| `portal_screenshot` | `path`, `region`, `display` | Capture a screenshot to a workspace file. |
| `portal_tools_reload` | none | Reload custom tools from `workspace/tools/mcp.toml`. |
| `portal_restart` | none | Restart a supervised Portal to load updated kits. |

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

Copy `portal.example.toml` to `portal.toml` and edit the local settings.
`portal.toml` is Git-ignored: do not commit machine-specific paths or tokens.
Relative workspace paths are resolved from the config file's directory. Portal
initializes that root before serving tools and stops on configuration/access
errors. On Windows, omitting the workspace uses
`%USERPROFILE%\.heart-portal\workspace`, rather than `/workspace`.

### 3. Run

```bash
./heart-portal --config portal.toml --connect "https://echo.beings.town/<being>/?token=<token>" --name "<machine-name>"
```

### macOS background recovery

Build from the checkout with `cargo build --release --locked`, then install a
per-user LaunchAgent (Python 3.9+ is required only for installation/management):

```bash
python3 scripts/portal-macos.py install --name "<original-machine-name>"
```

The installer reuses `.portal-connection.url` if present, otherwise prompts for
the Loom connection URL without echoing it. `PORTAL_CONNECT_LINK` is also
supported. On the first installation, use the original Portal name to retain
its relay identity. Later installations reuse the saved name and config.
Credentials are stored with owner-only permissions and are not placed in the
LaunchAgent plist or Portal command line.

The service starts immediately and at **user login**, without a terminal window
or `sudo`. macOS `launchd` restarts it after crashes, signals, and successful
`portal_restart` exits; rapid relaunches are throttled to a five-second launch
interval (not a fixed five-second delay after each exit). The existing network
reconnect backoff remains 2–30 seconds with jitter. Sleep suspends the service;
it reconnects after wake. This does not keep the Mac awake or detect arbitrary
process hangs.

Portal handles termination with up to ten seconds of managed-process cleanup;
launchd allows fifteen seconds before forcing shutdown. Runtime output goes
directly to files, so an inherited kit log handle cannot block recovery. The
previous launch's logs are retained as `portal-runtime.log.previous` and
`portal-runtime.err.log.previous`. macOS also rejects a second Portal for the
same user/relay/Being, including across checkouts and token rotations.

```bash
python3 scripts/portal-macos.py status
python3 scripts/portal-macos.py uninstall
# To update the binary, uninstall first, then build and install again:
cargo build --release --locked
python3 scripts/portal-macos.py install
```

Uninstall stops this checkout's service and preserves its config, credentials,
and name. Installation replaces any manually running Portal from this checkout.
It captures the current `PATH` for Node/Python and other kit commands; reinstall
after changing runtime locations. Kits continue to use `~/.heart-portal/kits`
or the configured `kits_dir`. No kits are installed automatically.

The LaunchAgent lives in `~/Library/LaunchAgents/town.beings.heart-portal.*.plist`.
Keep the checkout at its installed path; uninstall before moving it. macOS
privacy permissions still apply to background execution and screen capture.
If startup logs report access denied under Desktop/Documents, move the checkout
to a development directory outside those protected folders and reinstall.
Normal startup and config checks leave the executable's signature and file
attributes unchanged. A newly built ad-hoc-signed binary can still require fresh
macOS file permissions after an update; enable only the needed folders under
System Settings > Privacy & Security > Files & Folders.

Regression tests use a temporary checkout and a temporary real LaunchAgent;
they never connect to a Being:

```bash
python3 scripts/tests/macos-lifecycle.tests.py
```

### Windows background recovery

Build from the checkout with `cargo build --release --locked` (Rust MSVC
toolchain and the Visual Studio C++ Build Tools are required), then install:

```powershell
.\scripts\install-portal-windows.ps1 -ConnectLink "https://echo.beings.town/<being>/?token=<token>" -PortalName "<machine-name>"
```

For an existing Portal, use its original `--name` on the first installation.
If omitted, the first installation derives a name from the Being and computer
name. Later installations and restarts reuse the saved name and scheduled task.
Use a different name on each machine.

The installer creates a local config from `portal.example.toml` if absent.
It saves the connection link, name, and task in the Git-ignored
`.portal-connection.url`, `.portal-name`, and `.portal-task-name`.
The task starts at user logon through a windowless launcher; Windows Script
Host/VBScript must be available. Portal and its supervisor each reject a
duplicate instance for the same relay/Being in the current Windows session.

After updating a kit, the Being should call `portal_restart`, not kill
Portal or launch another supervisor. The tool is available only under supervision:
it returns a response, exits, and the supervisor relaunches Portal after five
seconds using the same name. Shutdown cleanup is limited to ten seconds;
inherited log pipes cannot hold the supervisor's restart loop indefinitely.
The existing relay reconnect backoff (2–30 seconds with jitter) is unchanged.

Kits remain under the current user's `~/.heart-portal/kits` or configured
`kits_dir`. Use `heart-portal --config portal.toml kit status` for
pre-flight checks and the runtime logs for startup errors. `portal_kit_usage`
counts successful calls since its last read; `{}` is not a kit inventory.

To update the Portal binary locally (kit-only updates do not need this):

```powershell
.\scripts\uninstall-portal-task.ps1
cargo build --release --locked
.\scripts\install-portal-task.ps1
```

Uninstall preserves the local config and saved identity. For removal only,
run the uninstall command without rebuilding/reinstalling. Unrestricted scripts
run as the same Windows user and can still stop the supervisor; this is recovery
from ordinary exits/crashes, not a security boundary against deliberate termination.

Windows recovery regression tests (temporary fixtures, no real relay/task changes):

```powershell
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts/tests/windows-lifecycle.tests.ps1
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

On Windows, `portal_exec` preserves quotes around executable paths such as
`"C:\Program Files\Git\usr\bin\bash.exe" -c "echo ok"`, including in background
mode. When Git for Windows is installed, Portal adds its Unix tools as a PATH
fallback without changing the configured shell or overriding existing commands.
See the [tool reference](starter-kit/guides/portal-ref.md) for shell quoting,
file `unescape` behavior, and environment/authentication troubleshooting.
For Chinese text in Windows PowerShell, use
`{"shell":"powershell","command":"Get-Content -LiteralPath '中文.txt'"}`;
this selects UTF-8 output/text defaults and transports the script without code-page
loss. `Path outside workspace` remains a boundary rejection, not a reason to
create a different workspace or retry via shell commands.

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

*Portal v0.6.0 — Being's hands in the world.*
