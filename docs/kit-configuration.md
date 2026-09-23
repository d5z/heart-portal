# Kit configuration and hot reload

Portal loads each kit from `<kits_dir>/<kit>/manifest.json` and reads `.env`
from the same directory. Installing the code does not authorize the kit to use
a third-party account. The account owner supplies credentials; the kit and the
service validate tokens, scopes and resource permissions. Portal's current
`portal_oauth_authorize` provider is OpenAI, not Jira.

Use `portal_kits_setup` with `{"kit":"jira"}` to inspect one kit's runtime,
dependencies, setup instructions and authentication alternatives. This returns
instructions and tool names; it does not run install commands or open URLs.

## Credentials

Grove's `provision.env` declares configuration requirements. Portal accepts the
list below and the legacy name-keyed form, such as
`{"OPENAI_API_KEY": {"required": true}}`:

```json
{
  "provision": {
    "env": [
      {"name": "JIRA_URL", "description": "Jira instance URL", "required": true},
      {"name": "JIRA_TOKEN", "description": "Personal access token for Bearer auth", "required": false}
    ]
  }
}
```

Supply values in the kit's local `.env`, for example:

```dotenv
JIRA_URL=https://jira.example.com
JIRA_TOKEN='replace-with-your-personal-access-token'
```

Portal injects the configured environment. Service-specific authentication,
API versions and request formats belong in the kit. Kits should report HTTP
errors and redirects as tool errors even when a response is HTML or empty.
`healthy` means the MCP process is running, not that every service operation is
authorized.

`not-started` means a process has not been started yet. A configured kit is
started on its first tool call. `eager: true` requests best-effort prewarming
at Portal startup, in the background with a bounded timeout. Failures are logged
and do not block Portal readiness; eager does not guarantee a running or healthy
kit. Kits discovered or reloaded later start on their next tool call, without
restarting Portal.
Use the exact tool name from `tools/list`. Use `portal_status` to distinguish
the actual local build from an older executable that shares its package version.

Configuration precedence is kit `.env`, then the optional string `default` in
a `provision.env` entry. Explicit empty values override defaults. Manifest
`provision.env` and `provision.auth.env` declarations never authorize inheritance
of host variables. Put required credentials in the kit-local `.env` or use the
kit-managed login flow. Runtime basics such as PATH and locale remain available;
they do not satisfy declared credential requirements without a local value.
Portal does not mutate its own environment,
so one kit's `.env` does not leak into other kits. Existing kits without
`provision.env` also receive `.env` values. `PORTAL_KIT_NAME` and `PORTAL_KIT_DIR`
are set by Portal. Unless the kit explicitly sets PATH, Unix PATH includes the
usual toolchain directories. Command resolution, preflight and startup use the
same kit PATH (and PATHEXT on Windows); relative PATH entries use the kit directory.
An explicit empty PATH is preserved, without falling back to host toolchains.

The dotenv parser supports quotes, comments, `export`, CRLF, UTF-8 BOM and
multiline quoted values. Values are literal: `$NAME` and `${NAME}` are never
expanded using Portal secrets or other dotenv entries. Quote tokens containing
whitespace or comment characters. `.env` must be a regular, non-symlink UTF-8
file of at most 256 KiB, with at most 256 variables.
Keep `.env` local, restrict file access to its owner, and exclude it from bundles
and source control. Portal's configuration status and parser errors never return
credential values. Portal suppresses raw kit stderr and MCP debug payloads;
kits must also protect any logs they write themselves.

Missing, empty or Grove placeholder (`{{YOUR_*}}` / `{{REDACTED}}`) values for
required entries produce `needs-configuration`. An unreadable/malformed `.env`
also blocks startup. These cases do not consume process restart attempts.
`portal_kits_status` returns variable descriptions, `required`/`configured`
flags, the `.env` path and a configuration error. A direct call to an
unconfigured kit explains what to configure before retrying.

## Extensible authentication

Existing manifests need no changes to use hot reload or `.env`. Kit authors can
optionally add `provision.auth`, a versioned Portal extension to Grove's existing
provision metadata, to describe authentication alternatives without adding
service-specific code to Portal:

```json
{
  "provision": {
    "env": [{"name": "SERVICE_URL", "required": true}],
    "auth": {
      "version": 1,
      "required": true,
      "methods": [
        {
          "id": "pat",
          "provider": "env",
          "flow": "bearer",
          "label": "Personal access token",
          "env": ["SERVICE_PAT"],
          "instructions": "Create a token in your account settings and save it in the kit .env."
        },
        {
          "id": "basic",
          "provider": "env",
          "flow": "basic",
          "env": ["SERVICE_EMAIL", "SERVICE_API_TOKEN"]
        }
      ]
    }
  }
}
```

Authorization URLs must use HTTPS, except HTTP on loopback hosts for local login callbacks.
Rejected URLs are marked invalid and omitted from setup/status URL fields.

Methods are alternatives: either PAT **or** email **and** API token satisfies
local preflight. Separately required `provision.env` entries still apply to all
methods. Leave alternative credentials out of global required entries, or mark
them `required:false`. Omit `auth` for legacy/no-auth kits; use `required:false`
when the kit supports useful anonymous operations.

| Provider | Requirements and behavior |
| --- | --- |
| `env` | All names in `env` must have nonempty, non-placeholder values. Supports API keys, bearer/PAT tokens, Basic credentials, etc. |
| `file` | All paths in `files` must be readable, nonempty regular, non-symlink credential files, at most 1 MiB each. Only paths within the kit directory are accepted; absolute paths, `..`, `~/` and links escaping that directory are rejected. External CLI credentials should use a kit-managed login flow instead of having Portal fingerprint host files. Contents are never returned. |
| `kit` | The kit owns OAuth, device-code, CLI, desktop login or another custom flow. Supply `tools`, `instructions` and/or `url` to guide setup. Optional `env`/`files` are prerequisites for bootstrapping the flow. |

Both `env` and `file` methods can combine environment and file requirements.
`flow` is an open informational label, not a hardcoded provider implementation.
For example, a self-contained OAuth kit can declare:

```json
{
  "auth": {
    "methods": [{
      "id": "account-login",
      "provider": "kit",
      "flow": "oauth",
      "tools": ["login", "auth_status"],
      "instructions": "Call login, complete authorization in the browser, then call auth_status."
    }]
  }
}
```

Place this `auth` block inside `provision`. Tool names must exist in the kit's
`tools` array; setup responses return their fully prefixed names. Kit-managed
auth is reported as `managed-by-kit`, allowing login tools to start before the
user is authenticated. It is never reported as an authorized account. File/env
checks report `configured`; expired tokens, scopes and API permissions still
require the kit/service's own validation.

Unknown provider types remain visible as `unsupported`. A required auth block
with no usable alternative blocks startup; a supported fallback can still work.
Unsupported auth protocol versions also fail explicitly. Local preflight checks
handle `env`, `file` and `kit` directly in `portal/src/kits/auth.rs`;
new services/kit-managed flows need only metadata.
Credential-file content changes participate in automatic hot reload.

Portal retains Grove `runtime`, `deps`, `install`, `post_install`, `platforms`
and `instructions` metadata, and exposes these through the setup tool. It keeps
unknown provision fields for forward-compatible refresh comparison, without
echoing arbitrary extension data or env default values in setup responses.

## Install and update

1. Download the bundle using Grove's setup guide and install dependencies.
2. Extract the kit into the configured `kits_dir`. When installing, finish its
   files before placing `manifest.json`; stage upgrades outside `kits_dir` where
   possible. Retain the user's local `.env` during upgrades.
3. Inspect `portal_kits_setup`, supply credentials or follow the kit login flow,
   then call `portal_kits_reload` with `{"kit":"<name>"}`.
4. Check the returned configuration status, then call a kit tool to verify
   service access.

| Change | How it takes effect |
| --- | --- |
| New kit directory / removed kit directory | Automatic scan every five seconds, or `portal_kits_reload` |
| Manifest/version/command or `.env` | Automatic scan every five seconds, or `portal_kits_reload` |
| Declared credential-file content | Automatic scan every five seconds, or `portal_kits_reload` |
| Code/dependency changes with an unchanged manifest | `portal_kits_reload`, or update the manifest version |
| Custom MCP tools | `portal_tools_reload` |

Reload results include additive `structuredContent` with `schema_version: 1`:
`kits.added`, `reloaded`, `removed`, and `retained_invalid` name the affected kits.
`portal_restart_required` is false and `activation` is `next-tool-call`.
`portal_kits_reload` keeps its existing status array in `content[0].text`;
`portal_tools_reload` affects only custom tools. A retained invalid manifest
means the old working configuration remains active, not that the edit applied.

Automatic refresh keeps unchanged processes running. Explicit reload with a
`kit` name changes only that kit; other kits remain subject to the independent
five-second scanner. Omitting `kit` explicitly reloads all kits, including
unhealthy ones. The next call starts
each kit with its current files and credentials. In-flight calls finish on their
old connection, which is then shut down; they cannot change the replacement
process's health. Usage counts survive refresh. An incomplete/invalid manifest
keeps the last loaded version until a valid manifest is available; remove the
whole kit directory to uninstall.
On Windows, a running child can lock its working directory. For a manual
uninstall, let active calls finish, reload that kit to retire the idle child,
then remove its directory before making another kit call.

Use `portal_status.capabilities` to check behavior on the actual build, and
each kit's `process_id`/`diagnostics` to distinguish a pending first call, MCP
startup failure and a tool-level service error. `not-started` after reload is
expected. Portal's MCP debug logs omit protocol payloads, including auth and
tool results. Raw kit stderr is also suppressed. A kit writing excessive output
is stopped and reports an MCP failure; it cannot fill the Portal log with its
raw output.

Portal advertises MCP `tools.listChanged: true` and sends
`notifications/tools/list_changed` to each initialized connection, including
idle connections and the WebSocket relay bridge. Clients need to handle that
notification by requesting `tools/list` again. No Portal process restart or
connection reset is required.

## Portal reliability and trust boundary

`portal_status`, `portal_kits_status` and `portal_kits_setup` are read-only
snapshots. They do not scan, restart processes or apply edits. Wait for the
scanner or explicitly reload the target after changing its files. `starting`
means an MCP handshake is in progress; it does not block status or other kits.
The `portal_` management namespace is reserved, including normalized aliases.
Conflicting kit names or tool routes are rejected while the existing owner is
retained. Ownership includes the installed directory and its loaded generation:
an invalid manifest, duplicate name or target-excluded directory cannot transfer
that ownership. Targeted reload checks conflicts before changing registry state,
so ambiguity fails closed and a second kit cannot silently take over a loaded route.

A shared relay/TCP connection handles up to 32 concurrent work requests and
reserves eight additional management requests. Responses may arrive out of
order and must be matched by JSON-RPC ID. A slow kit call does not hold up
status, reload or other kit requests on that connection. Client input is capped
at 16 MiB (64 KiB for authentication), and response writes have a ten-second
deadline. Disconnecting a client cancels its work and releases pending slots.

Startup and protocol I/O occur outside the shared kit registry lock. Startup
has a deadline and is cancelled when that generation is replaced. At most two
process generations per installed kit and 32 across kit and custom MCP processes may coexist. When
old calls still use both generations, further startup returns a capacity error
until they finish; it does not terminate other kits. Each connection accepts
at most 16 pending requests and rejects excess requests without cancelling
existing calls. MCP messages are limited to 8 MiB and pipe writes to five
seconds. Stdout and stderr have separate byte/line limits; malformed, closed
or excessive output affects that kit connection. Metadata reads are bounded
and reject special files. The inventory supports at most 128 manifests, each
256 KiB, 128 tools, 256 declared env entries, 16 auth methods and 16 credential
file references. A scan exceeding inventory limits retains the prior inventory.

Windows launches each kit suspended, assigns it to a non-breakaway Job Object
with a 32-process limit, then resumes it. This owns descendants even if the kit
launcher exits or Portal is forcibly terminated. macOS/Linux use a dedicated
process group for ordinary descendants and kill the group during managed
cleanup. This Unix cleanup is best-effort. Unix groups do not contain children that deliberately detach, and
SIGKILL of Portal does not execute cleanup. Retired connections stay owned
through shutdown; cleanup does not search for arbitrary matching process names.
Portal keeps the group leader unreaped until the one-time group signal, preventing
PGID reuse during managed cleanup. Windows enumerates all captured thread handles
before resuming, so an auxiliary thread cannot hide the suspended primary thread.

An async tool returning a job ID has finished its MCP request, even if a child
job is still running. On Windows that child remains in the kit's Job Object
and is terminated when the kit is reloaded or Portal stops. Wait for background
jobs to finish before reloading; kits must detect interrupted jobs instead of
leaving persisted status at `running`. A job ID does not guarantee survival
across reloads or host restarts.

These are lifecycle and protocol protections, **not an OS security sandbox**.
Kit code still runs as Portal's OS user and can access that user's files and
processes. Portal does not enforce per-Being ownership or turn manifest
permissions into filesystem/network policy. Enabled `portal_exec` also grants
host command execution to the connected Being. Thus this build cannot guarantee
that hostile code or unrestricted shell commands cannot affect Portal. Do not
run untrusted community code on the assumption that these limits provide that
guarantee. Enforcing that boundary requires a separate OS identity or sandbox
with explicitly granted resources and a separate host administration channel.
In particular, environment filtering does not prevent same-user kit code from
reading Portal credentials on disk or inspecting accessible host processes.
`portal_status` hides host paths and the Portal PID by default; only the local
administrator can expose them with `security.expose_host_details = true`.
`portal_status.capabilities.kit_isolation` exposes this limitation explicitly.
