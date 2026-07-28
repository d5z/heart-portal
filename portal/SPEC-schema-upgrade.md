# Portal File Tools Schema Upgrade

## Problem

The MCP schema for file tools doesn't expose all implemented parameters,
and the descriptions don't guide the being toward correct usage patterns.

Current gaps:
1. `portal_file_write` schema only has `path` + `content`, but file.rs implements `encoding` (utf8/base64) and `append` (bool)
2. `portal_file_edit` and `portal_file_append` exist as functions in file.rs but are NOT registered in mod.rs — neither in list_builtin_tools() schema NOR in call() dispatch. They are dead code.
3. Descriptions are generic ("Write content to a file") — don't guide the being away from shell escaping traps
4. `portal_exec` description doesn't warn about shell escaping pitfalls for file creation

## Changes Required (mod.rs only)

### 1. portal_file_write — expose hidden params + improve description

Change the existing portal_file_write tool registration in list_builtin_tools():

```rust
tools.push(ToolInfo {
    name: "portal_file_write".to_string(),
    description: "Write content to a file. PREFERRED over portal_exec for creating/writing files — avoids shell escaping issues with $, quotes, backslashes. Content is written as-is (only JSON string unescaping applies).".to_string(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path (relative to workspace root, or absolute within workspace)"
            },
            "content": {
                "type": "string",
                "description": "Content to write. Written as-is — use real newlines, not \n. For literal backslash in code (e.g. JS \n, regex \d), double it (\\n, \\d)."
            },
            "append": {
                "type": "boolean",
                "description": "If true, append to file instead of overwriting (default: false)"
            },
            "encoding": {
                "type": "string",
                "description": "Content encoding: 'utf8' (default) or 'base64' (for binary files)",
                "enum": ["utf8", "base64"]
            }
        },
        "required": ["path", "content"]
    }),
});
```

### 2. portal_file_edit — register as a NEW tool

Add to list_builtin_tools() inside the `if self.config.tools.file` block:

```rust
tools.push(ToolInfo {
    name: "portal_file_edit".to_string(),
    description: "Replace exact text in a file. PREFERRED over sed/awk — no shell escaping issues. Shows context around the replacement.".to_string(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path (relative to workspace root)"
            },
            "old_text": {
                "type": "string",
                "description": "Exact text to find (must match exactly, including whitespace)"
            },
            "new_text": {
                "type": "string",
                "description": "Replacement text"
            },
            "count": {
                "type": "integer",
                "description": "Number of occurrences to replace (default: 1, use -1 for all)"
            }
        },
        "required": ["path", "old_text", "new_text"]
    }),
});
```

Add match arm in call():
```rust
"portal_file_edit" => file::edit(&self.config, arguments).await,
```

### 3. portal_file_read — improve description

Change description to:
```
"Read a file's contents. Returns text for text files, base64-encoded data for images (png/jpg/gif/webp). SVG returned as text."
```

### 4. portal_exec — add shell escaping warning

Change description to:
```
"Execute a shell command. For creating/editing files, prefer portal_file_write and portal_file_edit — they avoid shell escaping issues with $, quotes, and backslashes."
```

## Files to modify
- `/Users/d5/heart-portal/portal/src/tools/mod.rs` — schema definitions + call() dispatch

## Verification
- `cargo build` in `/Users/d5/heart-portal/portal/`
- Confirm portal_file_edit appears in tool list after Portal restart
- Test portal_file_edit with a simple text replacement
