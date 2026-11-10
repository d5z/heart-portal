# Town-Client integration

The `main` branch supports both standalone Portal and Town-Client.
Town-Client pins this repository as a source submodule, builds it on each target
platform, and ships it inside the client release. No separate Portal release is
needed for client updates.

Both distributions use the same binary and build command; no desktop-only build
feature is required. Standalone releases default to `full-access`, including
existing configs that omit `security.permission_mode`. The bundled example config
also explicitly selects `full-access`. File access can leave the workspace and
commands run on the host without a desktop approval service. Tool enable flags,
an explicitly configured exec allowlist, and OS permissions still apply.

Town-Client passes `--permission-mode` on every launch, overriding the config's
mode with the user's saved `read-only`, `supervised`, `auto-review`, or
`full-access` policy. Client registration via `HEART_PORTAL_CLIENT_FILE` provides
scene commands and tool approvals. Absence of a client handler does not bypass
an explicitly selected restricted mode.

The local `subagent-setup` CLI is an explicit administrator operation and can
save the model configuration without a running desktop approval service. It
preserves the saved permission mode; remote `portal_subagent_setup` tool calls
still require approval in restricted modes. Cancelling setup or stopping Portal
cancels the installer and its child processes.

Runtime CLI overrides survive installation delegation and supervisor restarts
on macOS and Windows. Only `@context`, `@scenes`, and `@plugins` are desktop
commands (`@security-approve` is reserved internally). Other prefixes, such as
cmd's `@echo` and PowerShell's `@(...)`, remain ordinary shell input and follow
the selected shell permission policy.

The client supplies `HEART_PORTAL_CLIENT_MANAGED=1`, `HEART_PORTAL_SUPERVISED=1`
and an explicit `--config`. This launch stays in the client-owned installation
instead of moving to the standalone installation. These environment markers are
lifecycle coordination, not an authorization boundary.

The client stops and disables its OS supervisor, verifies the old engine exited,
then replaces the application and starts the bundled engine with the same config.
Portal's standalone upgrade command rejects client-managed launches. Standalone
Portal installation and upgrades otherwise retain upstream behavior.

`HEART_PORTAL_READY_NONCE` and `HEART_PORTAL_READY_FILE` identify the supervised
launch. Both macOS and Windows publish PID/nonce-bound connection telemetry;
readiness remains independent of cloud reachability. The client owns rollback.
