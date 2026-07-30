//! File tools — read, write, list.

use crate::config::PortalConfig;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use tracing::debug;

/// Resolve a path relative to workspace root. Prevent logical `..` traversal only.
pub(crate) fn resolve_path_logical(config: &PortalConfig, path_str: &str) -> Result<PathBuf> {
    let root = &config.security.workspace_root;
    let path = PathBuf::from(path_str);
    
    // Build the full path
    let full = if path.is_absolute() {
        path
    } else {
        root.join(path_str)
    };
    
    // Normalize: resolve . and .. components without requiring the file to exist.
    // This is critical for write operations on new files.
    let mut normalized = PathBuf::new();
    for component in full.components() {
        match component {
            std::path::Component::ParentDir => {
                // Pop the last component (go up one level)
                if !normalized.pop() {
                    anyhow::bail!("Path traversal: cannot go above root: {}", path_str);
                }
            }
            std::path::Component::CurDir => {
                // Skip "." — it's a no-op
            }
            other => {
                normalized.push(other);
            }
        }
    }
    
    // Verify the normalized path is within workspace root
    if !normalized.starts_with(root) {
        anyhow::bail!("Path outside workspace: {} (resolved to {})", path_str, normalized.display());
    }
    
    Ok(normalized)
}

/// Existing path: follow symlinks and ensure the real path stays under workspace.
fn resolve_existing_path(config: &PortalConfig, path_str: &str) -> Result<PathBuf> {
    let logical = resolve_path_logical(config, path_str)?;
    let root_canon = config.security.workspace_root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "workspace root cannot be canonicalized ({}): {}",
            config.security.workspace_root.display(),
            e
        )
    })?;
    if !logical.exists() {
        anyhow::bail!("Path does not exist: {}", path_str);
    }
    let c = logical.canonicalize().map_err(|e| {
        anyhow::anyhow!("path cannot be canonicalized ({}): {}", logical.display(), e)
    })?;
    if !c.starts_with(&root_canon) {
        anyhow::bail!(
            "Path outside workspace: {} (resolved to {})",
            logical.display(),
            c.display()
        );
    }
    Ok(c)
}

/// Write path: walk each path component; if a prefix exists, canonicalize it before creating parents
/// (prevents `create_dir_all` from following a symlink that escapes the workspace).
pub(crate) fn resolve_write_path(config: &PortalConfig, path_str: &str) -> Result<PathBuf> {
    let logical = resolve_path_logical(config, path_str)?;
    let root = &config.security.workspace_root;
    let root_canon = root.canonicalize().map_err(|e| {
        anyhow::anyhow!("workspace root cannot be canonicalized ({}): {}", root.display(), e)
    })?;
    let rel = logical.strip_prefix(root).map_err(|_| {
        anyhow::anyhow!("Path outside workspace: {}", path_str)
    })?;
    let mut cur = root_canon.clone();
    for comp in rel.components() {
        cur.push(comp);
        if cur.exists() {
            let c = cur.canonicalize().map_err(|e| {
                anyhow::anyhow!("path cannot be canonicalized ({}): {}", cur.display(), e)
            })?;
            if !c.starts_with(&root_canon) {
                anyhow::bail!(
                    "Path outside workspace: {} (resolved to {})",
                    logical.display(),
                    c.display()
                );
            }
            cur = c;
        }
    }
    Ok(cur)
}

/// Convert literal backslash escape sequences to their byte values.
/// Compensates for upstream DSL parsers that consume backslashes during JSON serialization.
pub(crate) fn unescape_backslash_sequences(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
                Some('0') => result.push('\0'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Image extensions → MIME type mapping.
fn image_mime_type(ext: &str) -> Option<&'static str> {
    match ext.to_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// Max image file size for base64 encoding (10MB — screenshots can be 4-6MB).
const MAX_IMAGE_SIZE: usize = 10 * 1024 * 1024;

pub async fn read(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments.get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;

    let path = resolve_existing_path(config, path_str)?;
    debug!("file_read: {}", path.display());

    // Check if this is an image file
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if let Some(mime) = image_mime_type(ext) {
        // Binary read + base64 for images
        let bytes = tokio::fs::read(&path).await
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        if bytes.len() > MAX_IMAGE_SIZE {
            anyhow::bail!("Image too large: {} bytes (max: {})", bytes.len(), MAX_IMAGE_SIZE);
        }

        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        // SVG files are text — return as text, not image
        if mime == "image/svg+xml" {
            let text = String::from_utf8_lossy(&bytes);
            return Ok(serde_json::json!({
                "content": [{ "type": "text", "text": text }]
            }));
        }

        debug!("file_read: returning image ({}, {} bytes)", mime, bytes.len());
        return Ok(serde_json::json!({
            "content": [
                { "type": "text", "text": format!("[Image: {} ({} bytes)]", path.display(), bytes.len()) },
                { "type": "image", "data": b64, "mimeType": mime }
            ]
        }));
    }

    // Text file path
    let content = tokio::fs::read_to_string(&path).await
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

    if content.len() > config.security.max_file_size {
        anyhow::bail!("File too large: {} bytes (max: {})", content.len(), config.security.max_file_size);
    }

    // Truncate large responses to avoid flooding the being's context
    const MAX_RESPONSE_CHARS: usize = 100_000; // 100KB

    // Line-based partial read
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let offset = arguments.get("offset")
        .and_then(super::value_as_u64)
        .map(|v| v as usize)
        .unwrap_or(0); // 0-based line number

    let limit = arguments.get("limit")
        .and_then(super::value_as_u64)
        .map(|v| v as usize);

    let (text, truncated) = if offset > 0 || limit.is_some() {
        let start = offset.min(total_lines);
        let end = limit.map(|l| (start + l).min(total_lines)).unwrap_or(total_lines);
        let slice = lines[start..end].join("\n");
        let header = format!("[lines {}-{} of {}]\n", start + 1, end, total_lines);
        (format!("{}{}", header, slice), start > 0 || end < total_lines)
    } else {
        // Existing truncation logic for full read
        if content.len() > MAX_RESPONSE_CHARS {
            (
                format!("{}...\n\n(truncated: showing {}/{} bytes. Use offset/limit to read specific sections.)",
                    &content[..MAX_RESPONSE_CHARS], MAX_RESPONSE_CHARS, content.len()),
                true
            )
        } else {
            (content, false)
        }
    };

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "truncated": truncated
    }))
}

pub async fn write(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments.get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;

    let raw_content = arguments.get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;

    let unescape = arguments.get("unescape")
        .and_then(super::value_as_bool)
        .unwrap_or(false);

    let append = arguments.get("append")
        .and_then(super::value_as_bool)
        .unwrap_or(false);

    let encoding = arguments.get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("utf8");

    // Decode content based on encoding
    let bytes: Vec<u8> = match encoding {
        "base64" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(raw_content)
                .map_err(|e| anyhow::anyhow!("Invalid base64 content: {}", e))?
        }
        _ => {
            let text = if unescape {
                unescape_backslash_sequences(raw_content)
            } else {
                raw_content.to_string()
            };
            text.into_bytes()
        }
    };

    if bytes.len() > config.security.max_file_size {
        anyhow::bail!("Content too large: {} bytes (max: {})", bytes.len(), config.security.max_file_size);
    }

    let path = resolve_write_path(config, path_str)?;
    let mode_label = if append { "append" } else { "write" };
    debug!("file_{}: {} ({} bytes, unescape={}, encoding={})", mode_label, path.display(), bytes.len(), unescape, encoding);

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    if append {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to open {} for append: {}", path.display(), e))?;
        file.write_all(&bytes).await
            .map_err(|e| anyhow::anyhow!("Failed to append to {}: {}", path.display(), e))?;
    } else {
        tokio::fs::write(&path, &bytes).await
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    }

    let verb = if append { "Appended" } else { "Written" };
    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": format!("{} {} bytes to {}", verb, bytes.len(), path.display()) }]
    }))
}

pub async fn list(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments.get("path")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            debug!("Missing path argument in file_list, using current directory");
            "."
        });

    let path = resolve_existing_path(config, path_str)?;
    debug!("file_list: {}", path.display());

    let mut entries = Vec::new();
    let mut dir = tokio::fs::read_dir(&path).await
        .map_err(|e| anyhow::anyhow!("Failed to list {}: {}", path.display(), e))?;

    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().await?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        entries.push(serde_json::json!({
            "name": name,
            "size": meta.len(),
            "is_dir": meta.is_dir(),
            "modified": modified
        }));
    }

    entries.sort_by(|a, b| {
        let na = a.get("name").and_then(|v| v.as_str()).unwrap_or_else(|| {
            debug!("Missing or invalid name field in directory entry");
            ""
        });
        let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or_else(|| {
            debug!("Missing or invalid name field in directory entry");
            ""
        });
        na.cmp(nb)
    });

    let text = serde_json::to_string(&entries)?;
    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }]
    }))
}

pub async fn edit(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments.get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;

    let old_text_raw = arguments.get("old_text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'old_text' argument"))?;

    let new_text_raw = arguments.get("new_text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'new_text' argument"))?;

    let unescape = arguments.get("unescape")
        .and_then(super::value_as_bool)
        .unwrap_or(false);

    let old_text = if unescape { unescape_backslash_sequences(old_text_raw) } else { old_text_raw.to_string() };
    let new_text = if unescape { unescape_backslash_sequences(new_text_raw) } else { new_text_raw.to_string() };

    // count can be -1 (replace all); keep signed i64 coercion (no HF-7 helper for i64)
    let max_replacements = arguments.get("count")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok())))
        .unwrap_or(1);

    let path = resolve_existing_path(config, path_str)?;
    debug!("file_edit: {} (count={})", path.display(), max_replacements);

    let content = tokio::fs::read_to_string(&path).await
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

    let match_count = content.matches(old_text.as_str()).count();
    if match_count == 0 {
        anyhow::bail!("text not found in file");
    }

    let (new_content, replaced) = if max_replacements < 0 {
        (content.replace(old_text.as_str(), new_text.as_str()), match_count)
    } else {
        let n = max_replacements as usize;
        if n == 1 && match_count > 1 {
            anyhow::bail!("multiple matches found — be more specific ({} occurrences). Use count=-1 for all.", match_count);
        }
        (content.replacen(old_text.as_str(), new_text.as_str(), n), n.min(match_count))
    };

    tokio::fs::write(&path, new_content.as_bytes()).await
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;

    let msg = if replaced == 1 {
        let pos = new_content.find(new_text.as_str()).unwrap_or(0);
        let start_line = new_content[..pos].lines().count();
        let new_lines = new_text.lines().count().max(1);
        let affected_lines = old_text.lines().count().max(1);
        let end_line = start_line + new_lines.max(affected_lines) - 1;
        let result_lines: Vec<&str> = new_content.lines().collect();
        let ctx_start = if start_line > 3 { start_line - 3 } else { 1 };
        let ctx_end = (end_line + 3).min(result_lines.len());
        let context: String = result_lines[ctx_start.saturating_sub(1)..ctx_end]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:4}| {}", ctx_start + i, l))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Replaced in {} (lines {}-{})\n{}", path.display(), start_line, end_line, context)
    } else {
        format!("Replaced {} occurrence(s) in {} ({} bytes)", replaced, path.display(), new_content.len())
    };

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": msg }]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortalConfig;

    fn test_config() -> PortalConfig {
        let mut config = PortalConfig::default();
        config.security.workspace_root = std::path::PathBuf::from("/workspace");
        config
    }

    #[test]
    fn test_resolve_relative_path() {
        let config = test_config();
        let result = resolve_path_logical(&config, "hello.txt").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/hello.txt"));
    }

    #[test]
    fn test_resolve_nested_path() {
        let config = test_config();
        let result = resolve_path_logical(&config, "subdir/file.md").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/subdir/file.md"));
    }

    #[test]
    fn test_resolve_dot_path() {
        let config = test_config();
        let result = resolve_path_logical(&config, "./hello.txt").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/hello.txt"));
    }

    #[test]
    fn test_reject_traversal_dotdot() {
        let config = test_config();
        assert!(resolve_path_logical(&config, "../etc/passwd").is_err());
    }

    #[test]
    fn test_reject_traversal_deep() {
        let config = test_config();
        assert!(resolve_path_logical(&config, "subdir/../../etc/passwd").is_err());
    }

    #[test]
    fn test_reject_absolute_outside() {
        let config = test_config();
        assert!(resolve_path_logical(&config, "/etc/passwd").is_err());
    }

    #[test]
    fn test_allow_absolute_inside() {
        let config = test_config();
        let result = resolve_path_logical(&config, "/workspace/file.txt").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/file.txt"));
    }

    #[test]
    fn test_reject_traversal_escape() {
        let config = test_config();
        assert!(resolve_path_logical(&config, "a/b/c/../../../../etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_escape_read_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("portal-file-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let workspace = tmp.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = tmp.join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let link = workspace.join("leak.txt");
        symlink(&outside, &link).unwrap();

        let mut config = test_config();
        config.security.workspace_root = workspace.clone();

        let err = resolve_existing_path(&config, "leak.txt").unwrap_err();
        assert!(
            err.to_string().contains("outside workspace") || err.to_string().contains("resolved to"),
            "{}",
            err
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_escape_write_parent_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("portal-file-test2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let workspace = tmp.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside_dir = tmp.join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let link_dir = workspace.join("nested");
        std::fs::create_dir_all(&link_dir).unwrap();
        let evil_parent = link_dir.join("evil");
        symlink(&outside_dir, &evil_parent).unwrap();

        let mut config = test_config();
        config.security.workspace_root = workspace.clone();

        let err = resolve_write_path(&config, "nested/evil/x.txt").unwrap_err();
        assert!(
            err.to_string().contains("outside workspace") || err.to_string().contains("resolved to"),
            "{}",
            err
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_newline() {
        assert_eq!(unescape_backslash_sequences("hello\\nworld"), "hello\nworld");
    }

    #[test]
    fn test_tab() {
        assert_eq!(unescape_backslash_sequences("col1\\tcol2"), "col1\tcol2");
    }

    #[test]
    fn test_carriage_return() {
        assert_eq!(unescape_backslash_sequences("line\\rend"), "line\rend");
    }

    #[test]
    fn test_literal_backslash() {
        assert_eq!(unescape_backslash_sequences("path\\\\file"), "path\\file");
    }

    #[test]
    fn test_unknown_sequence_preserved() {
        assert_eq!(unescape_backslash_sequences("test\\xvalue"), "test\\xvalue");
    }

    #[test]
    fn test_no_escapes() {
        assert_eq!(unescape_backslash_sequences("plain text 123"), "plain text 123");
    }

    #[test]
    fn test_trailing_backslash() {
        assert_eq!(unescape_backslash_sequences("end\\"), "end\\");
    }

    #[test]
    fn test_mixed() {
        assert_eq!(
            unescape_backslash_sequences("line1\\nline2\\ttab\\\\slash"),
            "line1\nline2\ttab\\slash"
        );
    }
}
