# Android APK host integration

Consume the `android-portal` branch as a Git submodule pinned to a tested commit.
Build its Rust workspace for `aarch64-linux-android` / `x86_64-linux-android`
with the NDK bionic toolchain and 16 KB page alignment
(`-C link-arg=-Wl,-z,max-page-size=16384`). Android DNS requires bionic/netd;
do not substitute a static musl executable.

## Shared core and Android boundary

MCP, relay authentication, heartbeat/reconnect, files, search, configuration and
process management remain shared with desktop. Android web fetching uses reqwest
because curl is not guaranteed. Bytes/time/redirects are bounded and validated
public DNS results are pinned on every redirect. Disabled file/search/web tools
reject direct calls as well as disappearing from discovery.

The APK owns foreground services, Android permission UI, storage and native OS
providers: calendar, contacts, notifications, location/geofences and Health
Connect. Kotlin app code stays in the integrating repository. This bridge does
not supply desktop screenshot or interpreter runtimes.

## Optional supervision

`HEART_PORTAL_PARENT_PID` opts into parent-death SIGKILL handling and a nonblocking
`runtime.lock` under the APK's private `HOME`. The parent is checked after signal
attachment to cover startup races. Shell children attach parent-death handling
and use kill-on-drop. Without this variable, existing Android launchers and
standalone commands such as `heart-portal --version` retain their startup behavior.

`HEART_PORTAL_READY_FILE` plus a fresh `HEART_PORTAL_READY_NONCE` opt into connection
state publication. Readers must check PID and nonce; no credentials or server
messages are published.

## Optional native tool bridge

The APK listens only on a random IPv4 loopback port, creates a fresh secret per
host session, and sets `HEART_ANDROID_PORT` and `HEART_ANDROID_TOKEN` on the Rust
child. Each connection carries one UTF-8 JSON line request and response:

```json
{"token":"<session secret>","method":"list"}
{"result":[{"name":"portal_android_calendar","description":"...","inputSchema":{"type":"object"}}]}
```

```json
{"token":"<session secret>","method":"call","name":"portal_android_calendar","arguments":{"action":"calendars"}}
{"result":{"calendars":[]}}
```

Errors use `{"error":{"code":"permission_required","message":"..."}}`.
Rust exposes successful results as MCP text plus `structuredContent`. Requests
are at most 256 KiB; complete responses including newline at most 1 MiB; total
call timeout is 25 seconds. APK implementations must authenticate before calling
tools, bound listener resources, validate the current identity, and independently
enforce local authorization plus OS permissions. Discovery is not authorization.
There is no Shell fallback.

With both bridge variables present, native tools augment discovery and embedded
unsupported OAuth/custom-runtime reload actions are withheld and rejected.
Without the variables, no native tools are advertised and standalone Android
custom/OAuth behavior remains available. Shell child environments exclude bridge
credentials, relay credentials and host-control nonces. This does not turn the
shared app UID into a Shell sandbox.

Writes should carry a host-defined idempotency key. A timeout does not establish
that a provider write failed: inspect host activity or retry identical arguments
using the same key under the APK's retry contract.

## Validation and updates

Build both Android ABIs and run isolated QA provider tests. Cover bridge
authentication/denial, standalone startup, parent cleanup and supervised restart;
validate device-dependent permissions and background behavior separately. Keep
the submodule pinned and rerun integration tests before changing its commit.
