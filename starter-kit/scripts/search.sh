#!/usr/bin/env bash
# Example: pass a query string; prints a one-line reminder to use portal_web_search from MCP.
set -euo pipefail
q="${1:-}"
if [[ -z "$q" ]]; then
  echo "usage: $0 <query>" >&2
  exit 1
fi
echo "Use portal_web_search with query: $q"
