# Custom tools

## Wrap a CLI with `portal_exec`

Arguments: `command` (required), optional `workdir`, `timeout_secs`, `background`.

Example: run a script in the workspace root and capture stdout.

## Custom MCP (stdio)

1. Add a small server under `tools/` (see `tools/examples/hello-tool.js`).
2. Register it in `tools/mcp.toml` with `command` + `args`.
3. Call `portal_tools_reload` after edits so Portal reconnects and picks up tools.
