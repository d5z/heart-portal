# Portal built-in tools

| Tool | Parameters | Returns | Example |
|------|------------|---------|---------|
| `portal_exec` | `command`, optional `workdir`, `timeout_secs`, `background` | Shell output or background session info | `{"command": "uname -a"}` |
| `portal_process` | `action` (`list` \| `poll` \| `log` \| `write` \| `kill`), optional `session_id`, `timeout_ms`, `offset`, `limit`, `data` | Session/output bytes | `{"action": "list"}` |
| `portal_file_read` | `path` | File text | `{"path": "notes.txt"}` |
| `portal_file_write` | `path`, `content` | Ack text | `{"path": "out.txt", "content": "hi"}` |
| `portal_file_list` | `path` | Directory listing | `{"path": "."}` |
| `portal_search` | `pattern`, optional `path`, `max_matches` | Ripgrep-style matches | `{"pattern": "TODO"}` |
| `portal_web_fetch` | `url`, optional `max_chars` | Fetched body (truncated) | `{"url": "https://example.com"}` |
| `portal_web_search` | `query`, optional `count` (default 5, max 10) | JSON array of `{title, url, snippet}` | `{"query": "rust async book", "count": 5}` |
| `portal_tools_reload` | (none) | Reload status for custom tools | `{}` |
