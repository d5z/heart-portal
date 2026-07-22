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

/// Unescape common backslash sequences. `\n` always becomes LF (0x0a), never CRLF.
fn unescape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
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

    // 可选参数
    let append = arguments.get("append")
        .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
        .unwrap_or(false);
    
    let encoding = arguments.get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("utf8");

    let escape = arguments.get("escape")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    
    let source = arguments.get("source")
        .and_then(|v| v.as_str());

    // 验证 encoding 参数
    if encoding != "utf8" && encoding != "base64" && encoding != "escaped" {
        anyhow::bail!("Invalid encoding '{}'. Must be 'utf8', 'base64', or 'escaped'", encoding);
    }

    // 获取要写入的内容
    let (content_bytes, source_info) = if let Some(source_path) = source {
        // 从 source 文件读取内容
        let source_resolved = resolve_path_logical(config, source_path)?;
        debug!("file_write: reading from source {}", source_resolved.display());
        
        let bytes = tokio::fs::read(&source_resolved).await
            .map_err(|e| anyhow::anyhow!("Failed to read source {}: {}", source_resolved.display(), e))?;
        
        (bytes, Some(source_resolved))
    } else {
        // 从 content 参数获取内容
        let content_str = arguments.get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument (required when no 'source' provided)"))?;

        let bytes = match encoding {
            "utf8" => {
                let text = if escape { unescape(content_str) } else { content_str.to_string() };
                text.into_bytes()
            }
            "escaped" => {
                unescape(content_str).into_bytes()
            }
            "base64" => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(content_str)
                    .map_err(|e| anyhow::anyhow!("Invalid base64 content: {}", e))?
            },
            _ => unreachable!() // 已在上面验证过
        };
        
        (bytes, None)
    };

    // 检查文件大小限制
    if content_bytes.len() > config.security.max_file_size {
        anyhow::bail!("Content too large: {} bytes (max: {})", content_bytes.len(), config.security.max_file_size);
    }

    let path = resolve_write_path(config, path_str)?;
    debug!("file_write: {} ({} bytes), append={}, encoding={}, escape={}", 
           path.display(), content_bytes.len(), append, encoding, escape);

    // 创建父目录
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // 写入文件
    if append {
        use tokio::fs::OpenOptions;
        use tokio::io::AsyncWriteExt;
        
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to open {} for append: {}", path.display(), e))?;
        
        file.write_all(&content_bytes).await
            .map_err(|e| anyhow::anyhow!("Failed to append to {}: {}", path.display(), e))?;
    } else {
        tokio::fs::write(&path, &content_bytes).await
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    }

    // 生成返回信息
    let message = if let Some(source_path) = source_info {
        if append {
            // 获取文件总大小用于显示
            let total_size = tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            format!("Appended {} bytes from {} to {} (total: {} bytes)", 
                   content_bytes.len(), source_path.display(), path.display(), total_size)
        } else {
            format!("Copied {} bytes from {} to {}", 
                   content_bytes.len(), source_path.display(), path.display())
        }
    } else {
        if append {
            // 获取文件总大小用于显示
            let total_size = tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            let encoding_info = match encoding { "base64" => " (decoded from base64)", "escaped" => " (escape sequences expanded)", _ => "" };
            format!("Appended {} bytes{} to {} (total: {} bytes)", 
                   content_bytes.len(), encoding_info, path.display(), total_size)
        } else {
            let encoding_info = match encoding { "base64" => " (decoded from base64)", "escaped" => " (escape sequences expanded)", _ => "" };
            format!("Written {} bytes{} to {}", 
                   content_bytes.len(), encoding_info, path.display())
        }
    };

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": message }]
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

    // 新增测试用例
    #[tokio::test]
    async fn test_write_with_content() {
        let tmp = std::env::temp_dir().join(format!("portal-write-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        
        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024; // 1MB

        let args = serde_json::json!({
            "path": "test.txt",
            "content": "Hello, World!"
        });

        let result = write(&config, args).await.unwrap();
        let content = std::fs::read_to_string(tmp.join("test.txt")).unwrap();
        assert_eq!(content, "Hello, World!");
        
        let response = result.get("content").unwrap().as_array().unwrap();
        let text = response[0].get("text").unwrap().as_str().unwrap();
        assert!(text.contains("Written 13 bytes"));
        
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_write_with_append() {
        let tmp = std::env::temp_dir().join(format!("portal-write-append-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        
        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024; // 1MB

        // 第一次写入
        let args1 = serde_json::json!({
            "path": "append_test.txt",
            "content": "First line\n"
        });
        write(&config, args1).await.unwrap();

        // 第二次追加
        let args2 = serde_json::json!({
            "path": "append_test.txt",
            "content": "Second line\n",
            "append": true
        });
        let result = write(&config, args2).await.unwrap();
        
        let content = std::fs::read_to_string(tmp.join("append_test.txt")).unwrap();
        assert_eq!(content, "First line\nSecond line\n");
        
        let response = result.get("content").unwrap().as_array().unwrap();
        let text = response[0].get("text").unwrap().as_str().unwrap();
        assert!(text.contains("Appended 12 bytes"));
        assert!(text.contains("total:") && text.contains("bytes"));
        
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_write_with_base64_encoding() {
        let tmp = std::env::temp_dir().join(format!("portal-write-base64-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        
        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024; // 1MB

        // "Hello, World!" 的 base64 编码
        let args = serde_json::json!({
            "path": "base64_test.txt",
            "content": "SGVsbG8sIFdvcmxkIQ==",
            "encoding": "base64"
        });

        let result = write(&config, args).await.unwrap();
        let content = std::fs::read_to_string(tmp.join("base64_test.txt")).unwrap();
        assert_eq!(content, "Hello, World!");
        
        let response = result.get("content").unwrap().as_array().unwrap();
        let text = response[0].get("text").unwrap().as_str().unwrap();
        assert!(text.contains("Written 13 bytes"));
        assert!(text.contains("decoded from base64"));
        
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_write_with_source() {
        let tmp = std::env::temp_dir().join(format!("portal-write-source-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        
        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024; // 1MB

        // 创建源文件
        let source_content = "Content from source file";
        std::fs::write(tmp.join("source.txt"), source_content).unwrap();

        let args = serde_json::json!({
            "path": "destination.txt",
            "source": "source.txt"
        });

        let result = write(&config, args).await.unwrap();
        let content = std::fs::read_to_string(tmp.join("destination.txt")).unwrap();
        assert_eq!(content, source_content);
        
        let response = result.get("content").unwrap().as_array().unwrap();
        let text = response[0].get("text").unwrap().as_str().unwrap();
        assert!(text.contains("Copied 24 bytes from"));
        assert!(text.contains("source.txt"));
        assert!(text.contains("destination.txt"));
        
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_write_with_source_and_append() {
        let tmp = std::env::temp_dir().join(format!("portal-write-source-append-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        
        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024; // 1MB

        // 创建目标文件
        std::fs::write(tmp.join("target.txt"), "Initial content\n").unwrap();
        
        // 创建源文件
        std::fs::write(tmp.join("source.txt"), "Appended content").unwrap();

        let args = serde_json::json!({
            "path": "target.txt",
            "source": "source.txt",
            "append": true
        });

        let result = write(&config, args).await.unwrap();
        let content = std::fs::read_to_string(tmp.join("target.txt")).unwrap();
        assert_eq!(content, "Initial content\nAppended content");
        
        let response = result.get("content").unwrap().as_array().unwrap();
        let text = response[0].get("text").unwrap().as_str().unwrap();
        assert!(text.contains("Appended 16 bytes from"));
        // 更健壮的测试 - 检查总字节数，但更宽容
        assert!(text.contains("total:") && text.contains("bytes"));
        
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_unescape_sequences() {
        assert_eq!(unescape(r"line1\nline2"), "line1\nline2");
        assert_eq!(unescape(r"col1\tcol2"), "col1\tcol2");
        assert_eq!(unescape(r"path\\to\\file"), "path\\to\\file");
        assert_eq!(unescape(r"hello\xworld"), r"hello\xworld");
        assert_eq!(unescape(r"trailing\"), "trailing\\");
        assert_eq!(unescape(r"a\rb"), "a\rb");
        // Ensure \n is LF (0x0a), not CRLF
        assert_eq!(unescape(r"\n").as_bytes(), &[0x0a]);
    }

    #[tokio::test]
    async fn test_write_escape_default_literal() {
        let tmp = std::env::temp_dir().join(format!("portal-write-escape-default-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024;

        let args = serde_json::json!({
            "path": "literal.txt",
            "content": "line1\\nline2"
        });
        write(&config, args).await.unwrap();

        let bytes = std::fs::read(tmp.join("literal.txt")).unwrap();
        assert!(bytes.contains(&0x5c), "should contain literal backslash");
        assert!(bytes.windows(2).any(|w| w == [0x5c, 0x6e]), "should contain 5c 6e");
        assert!(!bytes.contains(&0x0a), "should not contain real newline");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_write_escape_true() {
        let tmp = std::env::temp_dir().join(format!("portal-write-escape-true-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024;

        // newline
        write(&config, serde_json::json!({
            "path": "nl.txt",
            "content": "line1\\nline2",
            "escape": true
        })).await.unwrap();
        let bytes = std::fs::read(tmp.join("nl.txt")).unwrap();
        assert_eq!(bytes, b"line1\nline2");
        assert!(bytes.contains(&0x0a));

        // tab
        write(&config, serde_json::json!({
            "path": "tab.txt",
            "content": "col1\\tcol2",
            "escape": true
        })).await.unwrap();
        assert_eq!(std::fs::read(tmp.join("tab.txt")).unwrap(), b"col1\tcol2");

        // double backslash → single
        write(&config, serde_json::json!({
            "path": "bs.txt",
            "content": "path\\\\to\\\\file",
            "escape": true
        })).await.unwrap();
        assert_eq!(std::fs::read_to_string(tmp.join("bs.txt")).unwrap(), "path\\to\\file");

        // unknown sequence preserved
        write(&config, serde_json::json!({
            "path": "unk.txt",
            "content": "hello\\xworld",
            "escape": true
        })).await.unwrap();
        assert_eq!(std::fs::read_to_string(tmp.join("unk.txt")).unwrap(), "hello\\xworld");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_write_escape_ignored_for_base64() {
        let tmp = std::env::temp_dir().join(format!("portal-write-escape-b64-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024;

        // base64 of "Hello" — escape=true must not alter the base64 string before decode
        let args = serde_json::json!({
            "path": "b64.txt",
            "content": "SGVsbG8=",
            "encoding": "base64",
            "escape": true
        });
        write(&config, args).await.unwrap();
        assert_eq!(std::fs::read_to_string(tmp.join("b64.txt")).unwrap(), "Hello");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_write_missing_content_and_source() {
        let tmp = std::env::temp_dir().join(format!("portal-write-error-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        
        let mut config = test_config();
        config.security.workspace_root = tmp.clone();
        config.security.max_file_size = 1024 * 1024; // 1MB

        let args = serde_json::json!({
            "path": "test.txt"
        });

        let result = write(&config, args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing 'content' argument"));
        
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
