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
fn resolve_write_path(config: &PortalConfig, path_str: &str) -> Result<PathBuf> {
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

pub async fn read(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments.get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;

    let path = resolve_existing_path(config, path_str)?;
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

    let path = resolve_write_path(config, path_str)?;
    debug!("file_write: {} ({} bytes)", path.display(), content.len());

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
        let na = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let nb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        na.cmp(nb)
    });

    let text = serde_json::to_string(&entries)?;
    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }]
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
}
