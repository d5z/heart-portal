# Runtime logging

Portal defaults to `info`. Set `RUST_LOG=heart_portal=debug` temporarily for
troubleshooting; protocol bodies are not needed for normal diagnostics. ANSI
colors are disabled for redirected output, or when `NO_COLOR` is present.

Kit scans still run every five seconds. Unchanged validation/conflict warnings
are suppressed across successful scans. A changed or recurring problem is
reported again, and resolved problems generate an INFO event. Repeated failures
of the background scan itself are also suppressed until the error changes or the
scan recovers. State resets when Portal restarts.

On macOS, stdout redirected to a regular file (including legacy launchd logs)
is automatically managed by the runtime logger. The macOS launch scripts also
set `HEART_PORTAL_LOG_FILE` explicitly. This variable can select a log file on
other platforms; the parent directory must already exist. Terminal output and
supervisor pipes retain their streaming behavior when the variable is unset.

Managed tracing logs rotate during execution at 10 MiB, retaining the active
file and one `.previous` generation. A single oversized write is capped at
10 MiB. The existing log exporter includes both files. On the first rotation,
a pre-existing oversized log is preserved as `.previous`; it is replaced on
the next rotation. Historical logs are not proactively deleted.

This limit applies to Portal tracing output. Raw child-process output, stderr,
and separate supervisor logs do not pass through this writer. Existing installed
launchers and binaries must be upgraded for the new behavior to take effect.
