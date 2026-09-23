# Portal built-in tools

| Tool | Parameters | Returns | Example |
|------|------------|---------|---------|
| `portal_status` | (none) | Running version/build ID, effective configuration, live connection state and loaded kit summary; no credential values | `{}` |
| `portal_exec` | `command`, optional `shell`, `workdir`, `timeout_secs`, `background`, `output_encoding` | Shell output or background session info | `{"command": "uname -a"}` |
| `portal_process` | `action` (`list` \| `poll` \| `log` \| `write` \| `kill`), optional `session_id`, `timeout_ms`, `offset`, `limit`, `data` | Session/output bytes | `{"action": "list"}` |
| `portal_file_read` | `path` | File text | `{"path": "notes.txt"}` |
| `portal_file_write` | `path`, `content`, optional `append`, `encoding`, `unescape` | Ack text | `{"path": "out.txt", "content": "hi"}` |
| `portal_file_edit` | `path`, `old_text`, `new_text`, optional `count` (`-1` for all), `unescape` | Replacement context | `{"path": "out.txt", "old_text": "hi", "new_text": "hello"}` |
| `portal_file_list` | `path` | Directory listing | `{"path": "."}` |
| `portal_search` | `pattern`, optional `path`, `max_matches` | Ripgrep-style matches | `{"pattern": "TODO"}` |
| `portal_web_fetch` | `url`, optional `max_chars` | Fetched body (truncated) | `{"url": "https://example.com"}` |
| `portal_tools_reload` | (none) | Reload custom tools | `{}` |
| `portal_kits_status` | (none; kits enabled) | Kit status, credential variable names and configured flags; no values | `{}` |
| `portal_kits_setup` | `kit` | Runtime, dependencies, setup instructions and auth alternatives; no local credential values | `{"kit":"jira"}` |
| `portal_kits_reload` | optional `kit` | Reload kit code, manifests and credentials; return configuration status | `{"kit":"jira"}` |
| `portal_restart` | (none; supervised Portal only) | Restart acknowledgement, then supervisor relaunches Portal | `{}` |

## Inspect this running Portal

Call `portal_status` with `{}` first when diagnosing versions, configuration paths
or kit installation. It is always available and does not run commands, reload
configuration or start kits. The JSON result is in `content[0].text`.

`portal.version` is the package version; `portal.build_id` is the executable's
SHA-256 captured at startup, so two local builds named `0.8.1` can be distinguished.
`config` describes the configuration already loaded by this process, including
the selected path, resolved workspace/kits paths and the need to restart after
changing `portal.toml`. `connection.state` comes from the running transport.

Check `capabilities` for this build's reload behavior and `config.warnings` for
ignored settings. `portal_tools_reload` affects only custom tools;
`portal_kits_reload` reports added, reloaded, removed and retained-invalid kits.

After reload, `not-started` is expected until the next call, even for an eager
kit. Check kit `process_id`, `diagnostics.generation` and `last_call.outcome`.
A `tool-error` with a healthy process means MCP delivered an error result;
inspect that result to distinguish arguments, authentication and service errors.
Portal does not infer that a server is broken or a token corrupt from HTTP 500
or from token encoding. Never kill Portal to apply kit changes: that interrupts
the Being's connection. Controlled restart is for Portal updates/config changes.

`kits` summarizes loaded manifests without refreshing them. For missing kit
credentials, use `portal_kits_setup`; after installation or credential changes,
use `portal_kits_reload` or wait for the five-second scan. Configured credentials
do not prove Jira or another service has granted access. Tokens, Loom links,
environment values and credential file contents are omitted.

## Shell commands on Windows

`portal_exec` uses `cmd.exe`, for both foreground and background commands. Quote
paths and arguments that contain spaces; do not add another outer quote pair:

```json
{"command":"\"C:\\Program Files\\Git\\usr\\bin\\bash.exe\" -c \"echo ok\""}
```

Portal preserves the command's quotes, pipelines, redirection, and `&&`/`&`.
It discovers Git for Windows from the inherited PATH or standard system/per-user
install directories, and appends its `usr/bin` to the exec child's PATH. This
provides `ls`, `cat`, `grep`, and `rm` when Git is installed. Existing Windows
commands and user PATH entries take precedence; the shell remains `cmd.exe`.
Use an explicit `bash.exe -c` for Bash syntax. Custom/portable Git installations
can be discovered by adding their `cmd` or `bin` directory to Portal's PATH.

For Windows PowerShell and Chinese text, select `shell: "powershell"` and pass
the script directly (also supported with `background: true`):

```json
{"shell":"powershell","command":"Write-Output '中文输出'; Get-Content -LiteralPath '中文.txt'"}
```

Portal transports the script with PowerShell's UTF-16LE `-EncodedCommand`, sets
console/pipe text encoding to UTF-8, and defaults `Get-Content`, `Set-Content`,
`Add-Content`, and `Out-File` to UTF-8. Explicit `-Encoding` arguments still take
precedence. This option does not change system settings or execution policy.
Windows PowerShell may add a UTF-8 BOM when writing files; use `portal_file_write`
for exact UTF-8 contents. Binary/non-UTF-8 programs still require their own encoding
options. A nested `powershell -Command ...` inside the default cmd shell is rejected
with `PowerShell commands require shell='powershell'; pass the script directly`.
Use that shell value and send only the PowerShell script as `command`; Portal does
not rewrite nested commands because their quoting and code-page behavior is ambiguous.

Encoding is normalized at the boundary that owns each child pipe. Portal owns and
decodes pipes created by `portal_exec`/`portal_process`. A kit that starts another
process owns that internal pipe and must decode it before emitting valid UTF-8 MCP
JSON; structured MCP errors should carry failures instead of undecodable stderr.

`output_encoding` accepts `auto`, `utf8`, or Windows-only `oem`. PowerShell uses
UTF-8. Windows cmd `auto` selects UTF-8 or the system OEM code page per line; use
an explicit value for known encodings or output that must stream before a newline.
Every Portal response is normalized UTF-8 text.

When reading UTF-8 files directly in a separate PowerShell session, use
`Get-Content -LiteralPath '中文.txt' -Encoding UTF8` or
`[IO.File]::ReadAllText('中文.txt', [Text.Encoding]::UTF8)` and set
`[Console]::OutputEncoding = [Text.Encoding]::UTF8` before producing text output.

## File contents and backslashes

`unescape` defaults to `false` for file write/edit: text is written or matched
as received after normal JSON decoding. A JSON `\n` is already a newline.
Only set `unescape: true` when the received text contains literal backslash
sequences that you want converted a second time (`\\n`, `\\t`, `\\r`, `\\\\`).
Leave it false for Windows paths and source code containing backslashes.

For example, `{"path":"lines.txt","content":"first\\nsecond","unescape":true}`
writes two lines. Without `unescape`, that example writes a literal `\n`.
For binary files, use `encoding: "base64"`; `unescape` does not affect base64.

## Environment and service troubleshooting

New installations use `~/.heart-portal/portal.toml`. Existing explicit configs,
saved launch paths and portable `portal.toml` files remain supported. Inspect
`heart-portal config path` before editing; do not assume the working directory
or exe directory contains the active config. `config migrate --from <path>`
previews a copy into the user directory; `--profile <name>` separates multiple
Portals, and `--apply` writes the copy without restarting or changing the source.
Activate the returned path with `--config` at the next controlled restart.

Windows defaults to `%USERPROFILE%\.heart-portal\workspace` when no workspace
is configured (or `./workspace` if the profile is unavailable). Set the intended
directory explicitly in `portal.toml`, using a TOML literal string for backslashes:

```toml
workspace = 'C:\Users\you\Portal Workspace'
```

Relative workspace paths are resolved against the config file's directory.
On startup, Portal creates and resolves only the configured root; an empty,
inaccessible, or non-directory root causes a startup error. An explicitly supplied
missing config is an error, rather than a switch to defaults. A root deleted or
made inaccessible after startup remains an error until its configuration/access
is corrected; file tool requests do not repair or expand the root.

`Path outside workspace` is a boundary rejection. Choose a path within the
configured workspace; expanding that boundary requires the owner's decision.
Do not use shell commands or create another root to bypass a rejected file request.

If a tool response is lost, the operation's outcome is unknown. In particular,
do not automatically repeat a POST, publish, or other side effect: check the
destination state, use the service's idempotency mechanism if available, or get
receipt confirmation before retrying. For a known background session, inspect it
with `portal_process` rather than launching the same command again.

- Town API requests made with the Being's `http` primitive use Hearth's
  authenticated route. A direct `curl` from a Portal machine may return 401;
  use the authenticated route or the API's supported credentials. This is not
  an exec quoting problem and does not require broadening the trusted-IP list.
- WSL needs a working local installation/distribution; Portal does not install it.
- `heart-portal --config portal.toml kit status` inspects installed kits. An empty
  kits directory is a deployment state; `portal_kit_usage` is not an inventory.
- After installing or updating a kit, call `portal_kits_reload` to apply it
  immediately, then inspect `portal_kits_status`. Portal also scans installations,
  removals, manifests and `.env` every five seconds. Code-only changes require
  explicit reload or a manifest version change. Portal stays running.
- Store kit credentials in that kit's `.env`. Portal checks required
  `provision.env` entries and reports `needs-configuration` for missing values.
  Installing a kit does not grant access to Jira or another service: the user
  supplies authorized credentials, and the kit/service checks their permissions.
- Start only one Portal per relay/Being on a machine; Windows/macOS instance
  guards prevent competing connections.

### Adjust a kit without managing the Portal host

Use `portal_kits_reload {"kit":"<name>"}` after editing that kit. It changes
only the selected kit and starts its new code on the next call. Status/setup
queries are read-only. Do not restart or kill Portal to install or adjust kits.
An MCP failure, capacity error or service authorization error should be handled
within that kit; use `portal_status` to inspect the host separately.

The `portal_` namespace belongs to the host. Kits run as the same OS user and
are not sandboxed; enabling `portal_exec` gives the Being host command access.
Neither kit authorship nor the manifest defines a per-Being access boundary.
See [kit reliability and trust](../../docs/kit-configuration.md#portal-reliability-and-trust-boundary).
