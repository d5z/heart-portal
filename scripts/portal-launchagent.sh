#!/bin/sh
# launchd owns the lifetime; exec keeps Portal as its direct child.
set -eu
umask 077
root=$1
label=$2
if [ "${XPC_SERVICE_NAME:-}" != "$label" ]; then
    echo 'Start this service with portal-macos.py install, not this launcher directly.' >&2
    exit 1
fi
cd "$root"
# Read data, never source the credential file as shell code.
PORTAL_CONNECT_LINK=$(cat .portal-connection.url)
portal_name=$(cat .portal-name)
[ -n "$PORTAL_CONNECT_LINK" ] && [ -n "$portal_name" ]
export PORTAL_CONNECT_LINK
export HEART_PORTAL_SUPERVISED=1
export RUST_LOG="${RUST_LOG:-info}"
# Use files, not supervisor pipes: inherited kit output cannot delay recovery.
# Retain one previous launch for diagnosing crashes, with bounded generations.
for log in portal-runtime.log portal-runtime.err.log; do
    if [ -f "$log" ]; then mv -f "$log" "$log.previous"; fi
done
exec "$root/target/release/heart-portal" --config "$root/portal.toml" --name "$portal_name" \
    >portal-runtime.log 2>portal-runtime.err.log
