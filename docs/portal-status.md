# Runtime status for Being

Call the built-in MCP tool `portal_status` with `{}`. It is available on Windows,
macOS and Linux, including when exec, file, custom tools or kits are disabled.
No shell or access to configuration files is required. The tool is advertised
with `readOnlyHint: true`; it returns a JSON string in `content[0].text`.

Host details are private by default: `portal.pid`, `portal.executable` and the
paths under `config` return `null`. The local administrator may opt in with
the following setting in `portal.toml`, then restart Portal:

```toml
[security]
expose_host_details = true
```

This exposes the details to every connected client; a tool argument cannot enable
it. `capabilities.host_details_visible` reports the loaded choice. Build identity,
connection state and kit health remain available without this opt-in.

| Field | Meaning |
| --- | --- |
| `schema_version` | Response schema, currently `1`. Consumers should tolerate new fields. |
| `portal.name` | Effective instance name used by this process's MCP/relay connection. |
| `portal.version` | Package version, also returned in MCP `initialize.serverInfo.version`. |
| `portal.build_id` | `sha256:<full executable digest>` captured at startup; `null` if the file could not be read. |
| `portal.executable`, `os`, `arch`, `pid` | Running executable location, platform and process identity. |
| `portal.started_at_unix_secs`, `uptime_seconds` | Startup time in UTC epoch seconds and monotonic process uptime. |
| `connection` | Effective `relay` or `tcp` mode and in-memory transport state. The listener is `null` in relay mode. |
| `config` | Selected config path/source, whether a file was loaded, user directory, resolved workspace/kits paths and custom tool config location. |
| `config.warnings` | Unsupported/ignored or shadowed configuration fields, with fixed guidance and no field values. |
| `capabilities` | This build's reload scopes, automatic versus explicit changes, activation timing, config/restart support and authorization boundary. |
| `tools` | Loaded feature switches and cached custom tool count. |
| `security` | MCP token configured flag, allowlist entry count and file size limit; no token or command values. |
| `kits` | Enabled/hot-reload flags, scan interval and loaded manifests grouped by status, with names, versions and declared tool counts. |
| `supervision` | Whether controlled restart is supported and pending. This does not probe supervisor liveness. |

Read `capabilities` before relying on advice for another Portal installation.
`capabilities.tools_reload.kits` is false: `portal_tools_reload` affects only
custom tools. Kit changes use `portal_kits_reload`.
`kit_reload.activation` is `next-tool-call`, including eager kits installed or
reloaded after startup. `portal_config_hot_reload` remains false.

Each kit item includes `process_id` (only for a live MCP connection), a random
`diagnostics.generation` replaced when its loaded configuration is refreshed,
`last_started_at_unix_ms`, start duration and the last completed call outcome.
`last_call.outcome` is `success`, `tool-error` (a valid MCP result with `isError`),
`request-error` (valid JSON-RPC error), `timeout` (external outcome unknown),
or `mcp-error` (transport/process failure). A startup failure is recorded separately in
`last_lifecycle_error`. No kit response bodies or error text are stored here.
`next_action` suggests configuration, runtime/MCP inspection, tool-result
inspection or a tool call. A tool error does not by itself make a live MCP
process unhealthy, nor does one successful call prove all service permissions:
`service_authorization` is always `not-verified-by-portal`.

`generation` is a reload marker, not a code hash. Use explicit reload and a
read-only tool result to verify updated code. File mtime alone cannot prove
which code is loaded; a file newer than the kit process may need reload, while
an older file is not evidence of stale code. The relevant process is the kit
child, which can change while the Portal PID stays the same.

The status comes from the process handling the request. A replacement executable
on disk or a newly edited config does not change its startup snapshot. Comparing
`build_id` distinguishes local builds that share a version number; the digest
identifies binary content, not source provenance or authenticity. Signing or
repackaging an executable can also change its digest.

`connection.state` is `starting`, `connecting`, `connected`, `retrying`, `handshake_rejected`, `invalid`
or `listening`. `connected` means a relay handshake succeeded; `listening` means
the local MCP TCP listener was bound. An offline Being cannot query a disconnected
Portal through that connection; use the host's local `heart-portal status` command
on Windows/macOS, or the configured service manager on Linux, for supervisor
diagnostics in that situation.

`handshake_rejected` means the WebSocket connected but the relay returned
`ok: false` to the Portal handshake. It does not by itself prove an expired
credential. The engine still retries with backoff so server-side recovery can
restore the connection. The runtime log includes a bounded, redacted reason
when the server provides one; no raw handshake or response is recorded.

`config.snapshot` is `loaded-at-startup`: Portal does not reread `portal.toml`
during a query. `config.reload_requires_restart` applies to Portal configuration.
Kit installations, removals, manifests and credentials use hot reload instead.
The `kits` inventory reflects manifests already loaded by Portal, including the
previous usable manifest retained after an incomplete update. It is not a fresh
directory scan and does not list rejected manifests. Use `portal_kits_status` for cached configuration status or `portal_kits_setup`
for one kit's setup requirements. Apply edits using `portal_kits_reload` or wait
for the five-second scan. Status queries never launch kits, reload tools or reset counters.

Only explicitly selected diagnostic fields are returned. The response omits
Loom connection links, tokens, environment/default values, credential contents,
raw config/manifest data and arbitrary error strings. Instance/kit names remain
visible; Portal host paths/PID require the local opt-in above. Kit setup tools
still expose their own installation paths and kit process IDs. A configured credential flag describes
presence, not external authorization; the kit and service still validate access.

`capabilities.kit_isolation` reports admission limits, reserved management tools
and process ownership. `process_cleanup` distinguishes Windows `job-object`
from Unix `best-effort-process-group`; the latter does not contain deliberately
detached processes. `malicious_code_sandbox: false` and `same_os_user: true`
are intentional: lifecycle containment does not prevent same-user hostile code
or an unrestricted `portal_exec` command from affecting the host. See
[the kit trust boundary](kit-configuration.md#portal-reliability-and-trust-boundary).
