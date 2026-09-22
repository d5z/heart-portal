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
│   MCP tools     │              │   consciousness  │
└──────────────────┘              └──────────────────┘
```

Portal connects **outbound** to Hearth's relay endpoint — no port forwarding needed.

## Built-in Tools

| Tool | Parameters | Description |
|------|------------|-------------|
| `portal_status` | none | Read running version/build ID, capabilities, effective config and warnings, connection and kit process/call status; no credential values. |
| `portal_exec` | `command`, `workdir`, `timeout_secs`, `background` | Execute shell commands with allowlist-based security. |
| `portal_process` | `action`, `session_id`, `data` | Manage background command sessions. |
| `portal_file_read` | `path`, `offset`, `limit` | Read files from the workspace. |
| `portal_file_write` | `path`, `content` | Write files inside the workspace. |
| `portal_file_list` | `path` | List directory contents. |
| `portal_web_fetch` | `url` | Fetch content from a URL. |
| `portal_search` | `query` | Search text across the workspace. |
| `portal_screenshot` | `path`, `region`, `display` | Capture a screenshot to a workspace file. |
| `portal_tools_reload` | none | Reload custom tools without restarting Portal. |
| `portal_kits_status` | none | Inspect kit configuration requirements and status without returning credential values. |
| `portal_kits_setup` | `kit` | Inspect runtime, installation steps and alternative authentication methods. |
| `portal_kits_reload` | optional `kit` | Immediately reload kit code, manifests and credentials. |
| `portal_restart` | none | Restart a supervised Portal. Kit updates use `portal_kits_reload`. |

Kits are discovered from the configured `kits_dir` (default `~/.heart-portal/kits`).
Portal checks for installations, removals, manifest changes and `.env` changes
every five seconds and notifies connected clients. See
[Kit configuration and hot reload](docs/kit-configuration.md) for credentials
and updates that only change code.

Being can call `portal_status` with `{}` to inspect the running Portal, even
when exec, file and kit tools are disabled. See [Runtime status](docs/portal-status.md)
for fields, build identification and the distinction between configuration and
service authorization.

## Setup

New Windows installations and direct macOS/Linux launches default to
`~/.heart-portal/portal.toml` (Windows:
`%USERPROFILE%\.heart-portal\portal.toml`). Existing explicit configs and saved
launches keep their paths. Run `heart-portal config path` to see which config
will be used; see [configuration layout and migration](docs/configuration-layout.md)
before relocating an existing installation.

On Windows and macOS, the downloaded executable runs the installed copy in
`~/.heart-portal/runtime`. Logs, guardian scripts/state, locks and upgrade backups
stay there too. Stop an existing legacy installation before moving it; downloads
never overwrite an already installed version. OS login registrations use their
standard locations, and explicit workspace/kit paths are preserved.

### 1. Download

Download the latest binary from [Releases](https://github.com/d5z/heart-portal/releases).

| Platform | Binary |
|----------|--------|
| macOS (Apple Silicon) | `heart-portal-aarch64-apple-darwin` |
| macOS (Intel) | `heart-portal-x86_64-apple-darwin` |
| Linux (x86_64) | `heart-portal-x86_64-unknown-linux-musl` |
| Windows | `heart-portal-windows-x86_64.exe` |

```bash
# macOS / Linux
chmod +x heart-portal-*
mv heart-portal-* heart-portal
```

### 2. Configure

For a new installation, create `~/.heart-portal/portal.toml` from
`portal.example.toml` and edit the local settings. Preserve any existing config;
`heart-portal config path` shows which file this installation uses. See the
[migration guide](docs/configuration-layout.md) for legacy installations.
Relative workspace paths are resolved from the config file's directory. Portal
initializes that root before serving tools and stops on configuration/access
errors. On Windows, omitting the workspace uses
`%USERPROFILE%\.heart-portal\workspace`, rather than `/workspace`.

### 3. Run

```bash
./heart-portal --config ~/.heart-portal/portal.toml --connect "https://echo.beings.town/<being>/?token=<token>" --name "<machine-name>"
```

The macOS release remains a directly runnable binary with the same command
above. `python3 scripts/package-portal-macos.py` builds/signs that artifact locally.
Notarization is postponed.
See [macOS signing and upgrade validation](docs/macos-upgrade.md).

### macOS background recovery

Normal macOS startup automatically attaches a background supervisor while
keeping the original foreground Portal and its Terminal/app permission origin.
This also applies when an old `start.sh` starts the new binary after upgrading.
Python 3.9+ is required. Crashes and `portal_restart` are recovered; Ctrl+C or
`./heart-portal stop` stops supervision. `./heart-portal status` shows both PIDs.
This covers the current login session. Login startup uses the existing entry:

```bash
python3 scripts/portal-macos.py install --name "<original-machine-name>"
```

For an already downloaded executable, the same management command supports
`--root /path/to/installed-folder`. New installations store configuration in
`~/.heart-portal/portal.toml`; use `--config /absolute/path/portal.toml` for an
existing or migrated profile. Existing saved configurations remain usable. Name the binary
`heart-portal` or retain its published filename. This preserves its path and
signature. Direct foreground execution remains available and does not install
a LaunchAgent automatically.

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
# Upgrade a compatible signed installation through the coordinated worker:
target/release/heart-portal upgrade
target/release/heart-portal upgrade --status
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

Download just `heart-portal-windows-x86_64.exe` into a writable folder and run it.
The exe embeds its supervisor and updater: on the first normal launch it creates
`%USERPROFILE%\.heart-portal\portal.toml`, extracts the scripts, registers a current-user logon task, and
starts Portal under supervision. No installer, separate script download, Rust,
Python, VBScript, or Visual C++ redistributable is needed for Portal itself.
Windows 10/11's built-in Windows PowerShell 5.1 runs the embedded scripts.
Kits may still require their own runtimes.

```powershell
.\heart-portal-windows-x86_64.exe --connect "https://echo.beings.town/<being>/?token=<token>" --name "<machine-name>"
.\heart-portal-windows-x86_64.exe status
```

Double-clicking without a connection link starts local MCP on `127.0.0.1:9100`.
Connecting to a Being requires its Loom link. For an existing Portal, keep its
original `--name`; use different names on different computers.

Launch arguments, connection link, selected environment and working directory
are saved in `.portal-launch.json`. Keep this credential-bearing file private.
Subsequent launches reuse those settings and the existing process. The logon
task starts a hidden bootstrap; it runs after this Windows user logs in, not
before login. If Windows policy blocks task registration, current-session
supervision still starts and the CLI reports that logon recovery is unavailable.
Keep the exe and generated files together at their original location.

Use `stop` to stop both Portal and its supervisors and disable logon recovery.
Running the exe again re-enables the task and resumes supervision. To change
connection or launch arguments, stop first, then run with the new arguments.

```powershell
.\heart-portal-windows-x86_64.exe stop
.\heart-portal-windows-x86_64.exe
```

After updating a kit, call `portal_kits_reload` for that kit; Portal and its
relay connection stay running. Use `portal_restart` only after changing Portal
configuration or replacing the Portal binary, and never kill Portal or launch
another supervisor. The restart tool is available only under supervision: it
returns a response, exits, and the supervisor relaunches Portal after five
seconds using the same name. Shutdown cleanup is limited to ten seconds;
inherited log pipes cannot hold the supervisor's restart loop indefinitely.
The existing relay reconnect backoff (2–30 seconds with jitter) is unchanged.

Kits remain under the current user's `~/.heart-portal/kits` or configured
`kits_dir`. Use `heart-portal kit status` for
pre-flight checks and the runtime logs for startup errors. `portal_kit_usage`
counts successful calls since its last read; `{}` is not a kit inventory.

For macOS, keep using the installed binary's `--upgrade` or `upgrade`; a
pre-downloaded signed update can use `upgrade --file /path/to/new-portal`.
Inspect `upgrade --status`. Active LaunchAgents are coordinated during replacement;
The automatic session supervisor also pauses during replacement and restarts
through the same permission origin. Existing `start.sh` and manual entries remain
usable; starting the new version attaches supervision automatically. Normal
upgrades validate the new release’s Developer ID signature and report whether
the old identity is compatible, without blocking an identity change. Published v0.8.0
rewrites itself to ad-hoc on startup, so its first migration must use the new
binary's `upgrade --target /installed/heart-portal`;
that identity change may require one-time authorization. Notarization is a separate release check, postponed during local
validation. See [macOS upgrade details](docs/macos-upgrade.md).

To upgrade a running Windows Portal to the latest GitHub release:

```powershell
.\heart-portal-windows-x86_64.exe upgrade
.\heart-portal-windows-x86_64.exe upgrade --status
# Or apply an already downloaded newer exe through the same transaction:
.\heart-portal-windows-x86_64.exe upgrade --file C:\Downloads\new-portal.exe
```

The exe remains a directly runnable program; no installer is added. The upgrade
command downloads the asset for the exact release tag, validates it, and hands
off to a windowless update worker before exiting to release the exe file.
The command reports **accepted**, not completed; `upgrade --status` reports the
final result. `--upgrade` remains supported as a legacy spelling.

The updater and supervisor share an installation lock. While replacing files,
the updater prevents relaunches, stops only this installation, and retains a
backup. The supervisor then launches the new binary. Success requires the new
version to finish local initialization and remain alive for five seconds;
relay reachability is independent of this check. Failed startup rolls back.
The current user's config, Portal name, connection link and kits are preserved.

Each exe embeds its matching supervisor scripts. They are upgraded and rolled
back together with the binary. A small stable bootstrap remains running while
the supervisor implementation is replaced; if the update worker crashes, it
uses the saved recovery worker to restore the interrupted transaction.
Ordinary crash recovery, duplicate prevention and `portal_restart` still work.
See [Windows upgrade details](docs/windows-upgrade.md)
for recovery, status and packaging instructions.

For a local **source rebuild** (kit-only updates do not need this):

```powershell
.\scripts\uninstall-portal-task.ps1
cargo build --release --locked
.\scripts\install-portal-task.ps1
```

Uninstall preserves the local config and saved identity. For removal only,
run the uninstall command without rebuilding/reinstalling. Unrestricted scripts
run as the same Windows user and can still stop the supervisor; this is recovery
from ordinary exits/crashes, not a security boundary against deliberate termination.

Windows verification uses temporary fixtures. Package tests create and remove
their own scheduled tasks and do not connect to a real Being:

```powershell
powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts/tests/windows-lifecycle.tests.ps1
.\scripts\package-portal-windows.ps1
.\scripts\tests\windows-package.tests.ps1 -LocalOnly
.\scripts\tests\windows-upgrade-e2e.ps1
```

Your being now has hands on your machine! 🤲

### Upgrading from Cowork

The legacy Cowork web UI, file HTTP API, WebSocket file notifications, and HTTP
health endpoint have been removed. Portal no longer opens the extra HTTP port
(previously 9101 by default). Existing `[cowork]` configuration is ignored and
can be deleted. MCP file tools, relay connections, background-task callbacks,
and Windows/macOS process supervision continue to work.

Use `python3 scripts/portal-macos.py status` on macOS or the scheduled task status
on Windows to check the supervised process. Confirm relay connectivity through
the Portal logs or an MCP tool call; `/api/health` is no longer available.

## Security

- **File tool boundary**: File tools only access files within the configured workspace root. This does not sandbox `portal_exec` or community kit processes; both run as the host OS user.
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

On Windows, sub-agent processes retain `SystemRoot`, `SystemDrive`,
`ProgramFiles`, `ProgramFiles(x86)`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`,
`TEMP`, `TMP`, and `COMSPEC` in addition to `[subagent].env_passthrough`.
This also applies to existing configurations with an explicit passthrough list.
These values come from the current machine so Node can resolve DNS and pi can
find an existing Git Bash installation. Git Bash must still be installed;
for a custom shell location, configure pi's `shellPath` in its `settings.json`,
or make bash discoverable on the forwarded `PATH`.

For Chinese text in Windows PowerShell, use
`{"shell":"powershell","command":"Get-Content -LiteralPath '中文.txt'"}`;
this selects UTF-8 output/text defaults and transports the script without code-page
loss. `Path outside workspace` remains a boundary rejection, not a reason to
create a different workspace or retry via shell commands.

Command output supports `output_encoding: "auto" | "utf8" | "oem"` (`oem`
is Windows-only). The default uses UTF-8 for PowerShell and macOS/Linux. For
Windows cmd, `auto` prefers UTF-8 and otherwise uses the system OEM code page,
independently for each stdout/stderr line. Non-ASCII output without a newline
may wait until EOF or a 64KiB buffer limit; at the limit, the decoder locks its
choice until the next newline. Use an explicit encoding for interactive output
or ambiguous legacy bytes; mixed encodings within one line cannot be reliably
auto-detected. This decodes captured output, not arbitrary file contents or stdin.

Background output is normalized once to UTF-8. `portal_process` offsets,
`next_offset`, limits, and `total_output_bytes` count **normalized UTF-8 bytes**,
not OEM source bytes. Reuse `next_offset` for pagination; invalid character
offsets or a limit too small for the next character return an error (a limit
of at least 4 bytes fits any UTF-8 character). Normal process exit drains both
pipes before reporting completion, with a 5-second bound if descendants retain
the pipe handles; such descendants may still produce later output.

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
