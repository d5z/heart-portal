# Portal built-in tools

| Tool | Parameters | Returns | Example |
|------|------------|---------|---------|
| `portal_exec` | `command`, optional `shell`, `workdir`, `timeout_secs`, `background` | Shell output or background session info | `{"command": "uname -a"}` |
| `portal_process` | `action` (`list` \| `poll` \| `log` \| `write` \| `kill`), optional `session_id`, `timeout_ms`, `offset`, `limit`, `data` | Session/output bytes | `{"action": "list"}` |
| `portal_file_read` | `path` | File text | `{"path": "notes.txt"}` |
| `portal_file_write` | `path`, `content`, optional `append`, `encoding`, `unescape` | Ack text | `{"path": "out.txt", "content": "hi"}` |
| `portal_file_edit` | `path`, `old_text`, `new_text`, optional `count` (`-1` for all), `unescape` | Replacement context | `{"path": "out.txt", "old_text": "hi", "new_text": "hello"}` |
| `portal_file_list` | `path` | Directory listing | `{"path": "."}` |
| `portal_search` | `pattern`, optional `path`, `max_matches` | Ripgrep-style matches | `{"pattern": "TODO"}` |
| `portal_web_fetch` | `url`, optional `max_chars` | Fetched body (truncated) | `{"url": "https://example.com"}` |
| `portal_web_search` | `query`, optional `count` (default 5, max 10) | JSON array of `{title, url, snippet}` | `{"query": "rust async book", "count": 5}` |
| `portal_tools_reload` | (none) | Reload status for custom tools | `{}` |
| `portal_restart` | (none; supervised Portal only) | Restart acknowledgement, then supervisor relaunches Portal | `{}` |

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
options. A nested `powershell -Command ...` inside the default cmd shell does not
receive these PowerShell defaults.

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
- Under supervision, use `portal_restart` after kit updates. Start only one
  Portal per relay/Being on a machine; Windows/macOS instance guards prevent
  competing connections.
