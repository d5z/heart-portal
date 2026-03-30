//! File tools — read, write, list.

use crate::config::PortalConfig;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use tracing::debug;

/// Resolve a path relative to workspace root. Prevent path traversal.
fn resolve_path(config: &PortalConfig, path_str: &str) -> Result<PathBuf> {
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

pub async fn read(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments.get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;

    let path = resolve_path(config, path_str)?;
    debug!("file_read: {}", path.display());

    let content = tokio::fs::read_to_string(&path).await
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

    if content.len() > config.security.max_file_size {
        anyhow::bail!("File too large: {} bytes (max: {})", content.len(), config.security.max_file_size);
    }

    // Truncate large responses to avoid flooding the being's context
    const MAX_RESPONSE_CHARS: usize = 100_000; // 100KB
    let (text, truncated) = if content.len() > MAX_RESPONSE_CHARS {
        (
            format!("{}...\n\n(truncated: showing {}/{} bytes. Use portal_exec with head/tail for specific sections.)",
                &content[..MAX_RESPONSE_CHARS], MAX_RESPONSE_CHARS, content.len()),
            true
        )
    } else {
        (content, false)
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

    let content = arguments.get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;

    if content.len() > config.security.max_file_size {
        anyhow::bail!("Content too large: {} bytes (max: {})", content.len(), config.security.max_file_size);
    }

    let path = resolve_path(config, path_str)?;
    debug!("file_write: {} ({} bytes)", path.display(), content.len());

    // Create parent dirs
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(&path, content).await
        .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": format!("Written {} bytes to {}", content.len(), path.display()) }]
    }))
}

pub async fn list(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments.get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let path = resolve_path(config, path_str)?;
    debug!("file_list: {}", path.display());

    let mut entries = Vec::new();
    let mut dir = tokio::fs::read_dir(&path).await
        .map_err(|e| anyhow::anyhow!("Failed to list {}: {}", path.display(), e))?;

    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().await?;
        entries.push(serde_json::json!({
            "name": name,
            "type": if meta.is_dir() { "directory" } else { "file" },
            "size": meta.len()
        }));
    }

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&entries)? }]
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
        let result = resolve_path(&config, "hello.txt").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/hello.txt"));
    }

    #[test]
    fn test_resolve_nested_path() {
        let config = test_config();
        let result = resolve_path(&config, "subdir/file.md").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/subdir/file.md"));
    }

    #[test]
    fn test_resolve_dot_path() {
        let config = test_config();
        let result = resolve_path(&config, "./hello.txt").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/hello.txt"));
    }

    #[test]
    fn test_reject_traversal_dotdot() {
        let config = test_config();
        assert!(resolve_path(&config, "../etc/passwd").is_err());
    }

    #[test]
    fn test_reject_traversal_deep() {
        let config = test_config();
        assert!(resolve_path(&config, "subdir/../../etc/passwd").is_err());
    }

    #[test]
    fn test_reject_absolute_outside() {
        let config = test_config();
        assert!(resolve_path(&config, "/etc/passwd").is_err());
    }

    #[test]
    fn test_allow_absolute_inside() {
        let config = test_config();
        let result = resolve_path(&config, "/workspace/file.txt").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/workspace/file.txt"));
    }

    #[test]
    fn test_reject_traversal_escape() {
        let config = test_config();
        assert!(resolve_path(&config, "a/b/c/../../../../etc/passwd").is_err());
    }
}
