# Heart Portal Extensions

Heart Portal implements a dual-layer architecture:

- **Kernel**: Built-in tools (exec, file, web) that are always available and never restart
- **Extensions**: External MCP servers that can be hot-reloaded without restarting Portal

## Configuration

Extensions are configured in `extensions.toml` in the workspace root:

```toml
[extensions.my-extension]
description = "My custom extension"
command = ["python3", "my_extension.py"]
working_dir = "extensions/my-extension"
auto_start = true
startup_timeout = 30
restart_on_crash = true

[extensions.my-extension.env]
DEBUG = "1"
API_KEY = "secret"
```

### Configuration Options

- `description`: Human-readable description of the extension
- `command`: Array of command and arguments to start the extension
- `working_dir`: Working directory relative to workspace root (optional)
- `auto_start`: Whether to start the extension when Portal starts (default: true)
- `startup_timeout`: Timeout in seconds for extension startup (default: 30)
- `restart_on_crash`: Whether to restart if the extension crashes (default: true)
- `env`: Environment variables to set for the extension process

## Management API

Portal provides JSON-RPC methods for managing extensions:

### `extensions/status`

Get status of all extensions:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "extensions/status"
}
```

Response:
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "extensions": {
      "my-extension": ["Running", null],
      "other-extension": ["Failed", "Connection refused"]
    }
  }
}
```

### `extensions/reload`

Hot-reload extensions configuration:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "extensions/reload"
}
```

This will:
1. Re-read `extensions.toml`
2. Stop extensions that are no longer configured
3. Start new extensions with `auto_start = true`
4. Restart extensions whose configuration changed

### `extensions/start`

Start a specific extension:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "extensions/start",
  "params": {
    "name": "my-extension"
  }
}
```

### `extensions/stop`

Stop a specific extension:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "extensions/stop",
  "params": {
    "name": "my-extension"
  }
}
```

### `extensions/restart`

Restart a specific extension:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "extensions/restart",
  "params": {
    "name": "my-extension"
  }
}
```

## Extension Development

Extensions are standard MCP servers that communicate via JSON-RPC. They should:

1. Accept JSON-RPC requests on stdin
2. Send JSON-RPC responses on stdout
3. Implement standard MCP protocol methods:
   - `initialize`
   - `tools/list`
   - `tools/call`

## Hot Reload

The extension system supports hot reload without restarting Portal:

1. Edit `extensions.toml`
2. Call `extensions/reload` via JSON-RPC
3. Portal will automatically apply changes:
   - New extensions are started
   - Removed extensions are stopped
   - Changed extensions are restarted
   - Tool list is updated

After reload, Portal closes MCP client connections to trigger re-discovery of tools.

## Architecture Benefits

- **Stability**: Kernel tools never go down
- **Flexibility**: Extensions can be updated independently
- **Development**: Fast iteration on extensions without Portal restart
- **Isolation**: Extension crashes don't affect kernel or other extensions
- **Scalability**: Add new capabilities without modifying Portal core