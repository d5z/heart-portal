//! File tools — read, write, list.

use crate::config::PortalConfig;
use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tracing::debug;

// Serialize mutations of the same canonical file, while unrelated files remain concurrent.
fn mutation_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

async fn bounded_bytes(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let path = path.to_path_buf();
    Ok(tokio::task::spawn_blocking(move || crate::bounded_file::read(&path, limit)).await??)
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn check_revision(arguments: &Value, bytes: &[u8]) -> Result<()> {
    if let Some(expected) = arguments.get("expected_sha256") {
        anyhow::ensure!(
            expected.as_str() == Some(checksum(bytes).as_str()),
            "File changed since it was read. Read it again and reapply only the intended edits."
        );
    }
    Ok(())
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    struct Temporary(PathBuf);
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let temp =
        Temporary(path.with_file_name(format!(".portal-write-{}.tmp", uuid::Uuid::new_v4())));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp.0)
        .await?;
    if let Ok(metadata) = tokio::fs::metadata(path).await {
        file.set_permissions(metadata.permissions()).await?;
    }
    file.write_all(bytes).await?;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&temp.0, path).await?;
    Ok(())
}

fn write_result(path: &Path, bytes: &[u8], message: String) -> Value {
    let link = url::Url::from_file_path(path)
        .ok()
        .map(|url| format!("[预览文件]({url})"));
    serde_json::json!({
        "content": [{"type":"text", "text": format!("{}{}", message, link.as_ref().map(|link| format!("\n{link}")).unwrap_or_default())}],
        "path": path, "bytes": bytes.len(), "sha256": checksum(bytes), "preview_link": link
    })
}

/// File/search tools are exposed only after `PortalConfig::prepare_workspace`.
/// Reject an unprepared relative root instead of trying to reinterpret it
/// relative to the process working directory and weakening the sandbox.
fn prepared_workspace_root(config: &PortalConfig) -> Result<&Path> {
    let root = config.security.workspace_root.as_path();
    anyhow::ensure!(!root.as_os_str().is_empty(), "workspace root is empty");
    anyhow::ensure!(
        root.is_absolute(),
        "workspace root is not prepared: {}",
        root.display()
    );
    Ok(root)
}

pub(crate) fn canonical_workspace_root(config: &PortalConfig) -> Result<PathBuf> {
    let root = prepared_workspace_root(config)?;
    let canonical = root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "workspace root cannot be canonicalized ({}): {}",
            root.display(),
            e
        )
    })?;
    anyhow::ensure!(canonical.is_dir(), "workspace root is not a directory");
    Ok(canonical)
}

/// Resolve a path relative to workspace root. Prevent logical `..` traversal only.
pub(crate) fn resolve_path_logical(config: &PortalConfig, path_str: &str) -> Result<PathBuf> {
    let root = prepared_workspace_root(config)?;
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
        anyhow::bail!(
            "Path outside workspace: {} (resolved to {})",
            path_str,
            normalized.display()
        );
    }

    Ok(normalized)
}

/// Existing path: follow symlinks and ensure the real path stays under workspace.
pub(crate) fn resolve_existing_path(config: &PortalConfig, path_str: &str) -> Result<PathBuf> {
    let logical = resolve_path_logical(config, path_str)?;
    let root_canon = canonical_workspace_root(config)?;
    if !logical.exists() {
        anyhow::bail!("Path does not exist: {}", path_str);
    }
    let c = logical.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "path cannot be canonicalized ({}): {}",
            logical.display(),
            e
        )
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
    let root = prepared_workspace_root(config)?;
    let root_canon = canonical_workspace_root(config)?;
    let rel = logical
        .strip_prefix(root)
        .map_err(|_| anyhow::anyhow!("Path outside workspace: {}", path_str))?;
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
    let path_str = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;

    let path = resolve_existing_path(config, path_str)?;
    debug!("file_read: {}", path.display());

    // Check if this is an image file
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    // SVG is text: apply the same output budget, partial reads and revision hash.
    if let Some(mime) = image_mime_type(ext).filter(|mime| *mime != "image/svg+xml") {
        // Binary read + base64 for images
        let bytes = bounded_bytes(&path, MAX_IMAGE_SIZE.min(config.security.max_file_size))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        if bytes.len() > MAX_IMAGE_SIZE {
            anyhow::bail!(
                "Image too large: {} bytes (max: {})",
                bytes.len(),
                MAX_IMAGE_SIZE
            );
        }

        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        debug!(
            "file_read: returning image ({}, {} bytes)",
            mime,
            bytes.len()
        );
        return Ok(serde_json::json!({
            "content": [
                { "type": "text", "text": format!("[Image: {} ({} bytes)]", path.display(), bytes.len()) },
                { "type": "image", "data": b64, "mimeType": mime }
            ]
        }));
    }

    // Text file path
    let content = String::from_utf8(bounded_bytes(&path, config.security.max_file_size).await?)
        .map_err(|e| anyhow::anyhow!("File is not UTF-8 text ({}): {}", path.display(), e))?;

    if content.len() > config.security.max_file_size {
        anyhow::bail!(
            "File too large: {} bytes (max: {})",
            content.len(),
            config.security.max_file_size
        );
    }

    // Truncate large responses to avoid flooding the being's context
    let max_response_bytes = arguments
        .get("max_bytes")
        .and_then(super::value_as_u64)
        .unwrap_or(100_000)
        .clamp(1, 100_000) as usize;
    let sha256 = checksum(content.as_bytes());

    // Line-based partial read
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let offset = arguments
        .get("offset")
        .and_then(super::value_as_u64)
        .map(|v| usize::try_from(v).unwrap_or(usize::MAX))
        .unwrap_or(0); // 0-based line number

    let limit = arguments
        .get("limit")
        .and_then(super::value_as_u64)
        .map(|v| usize::try_from(v).unwrap_or(usize::MAX));

    let (text, truncated) = if offset > 0 || limit.is_some() {
        let start = offset.min(total_lines);
        let end = limit
            .map(|l| start.saturating_add(l).min(total_lines))
            .unwrap_or(total_lines);
        let mut slice = String::new();
        let mut clipped = false;
        for (index, line) in lines[start..end].iter().enumerate() {
            let separator = if index == 0 { "" } else { "\n" };
            if slice.len() + separator.len() + line.len() > max_response_bytes {
                let remaining = max_response_bytes.saturating_sub(slice.len() + separator.len());
                if remaining > 0 {
                    slice.push_str(separator);
                    slice.push_str(super::text::byte_prefix(line, remaining));
                }
                clipped = true;
                break;
            }
            slice.push_str(separator);
            slice.push_str(line);
        }
        let header = format!(
            "[requested lines {}-{} of {}]\n",
            start.saturating_add(1),
            end,
            total_lines
        );
        let notice = if clipped {
            "\n(truncated by max_bytes; narrow offset/limit or increase max_bytes up to 100000.)"
        } else {
            ""
        };
        (
            format!("{}{}{}", header, slice, notice),
            clipped || start > 0 || end < total_lines,
        )
    } else {
        // Existing truncation logic for full read
        if content.len() > max_response_bytes {
            let prefix = super::text::byte_prefix(&content, max_response_bytes);
            (
                format!("{}...\n\n(truncated: showing {}/{} bytes. Use offset/limit to read specific sections.)",
                    prefix, prefix.len(), content.len()),
                true
            )
        } else {
            (content, false)
        }
    };

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "truncated": truncated,
        "path": path, "sha256": sha256, "total_lines": total_lines
    }))
}

pub async fn write(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;

    let raw_content = arguments
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'content' argument"))?;

    let unescape = arguments
        .get("unescape")
        .and_then(super::value_as_bool)
        .unwrap_or(false);

    let append = arguments
        .get("append")
        .and_then(super::value_as_bool)
        .unwrap_or(false);

    let encoding = arguments
        .get("encoding")
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
        anyhow::bail!(
            "Content too large: {} bytes (max: {})",
            bytes.len(),
            config.security.max_file_size
        );
    }

    let path = resolve_write_path(config, path_str)?;
    let lock = mutation_lock(&path);
    let _guard = lock.lock().await;
    if arguments.get("expected_sha256").is_some() {
        check_revision(
            &arguments,
            &bounded_bytes(&path, config.security.max_file_size).await?,
        )?;
    }
    let mode_label = if append { "append" } else { "write" };
    debug!(
        "file_{}: {} ({} bytes, unescape={}, encoding={})",
        mode_label,
        path.display(),
        bytes.len(),
        unescape,
        encoding
    );

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
        file.write_all(&bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to append to {}: {}", path.display(), e))?;
    } else {
        atomic_write(&path, &bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    }

    let verb = if append { "Appended" } else { "Written" };
    if append {
        return Ok(
            serde_json::json!({"content":[{"type":"text","text":format!("{} {} bytes to {}",verb,bytes.len(),path.display())}],"path":path,"bytes_written":bytes.len()}),
        );
    }
    Ok(write_result(
        &path,
        &bytes,
        format!("{} {} bytes to {}", verb, bytes.len(), path.display()),
    ))
}

pub async fn list(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            debug!("Missing path argument in file_list, using current directory");
            "."
        });

    let path = resolve_existing_path(config, path_str)?;
    debug!("file_list: {}", path.display());

    let mut entries = Vec::new();
    let mut dir = tokio::fs::read_dir(&path)
        .await
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
    let path_str = arguments
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Missing 'path' argument"))?;
    let path = resolve_existing_path(config, path_str)?;
    let lock = mutation_lock(&path);
    let _guard = lock.lock().await;
    let bytes = bounded_bytes(&path, config.security.max_file_size).await?;
    check_revision(&arguments, &bytes)?;
    let mut content = String::from_utf8(bytes)?;
    let edits = match arguments.get("edits") {
        Some(Value::Array(edits)) if !edits.is_empty() && edits.len() <= 64 => {
            edits.iter().collect::<Vec<_>>()
        }
        Some(_) => anyhow::bail!("edits must contain between 1 and 64 exact-text replacements"),
        None => vec![&arguments],
    };
    let mut replaced = 0usize;
    let mut changed_line = 1usize;
    for (index, edit) in edits.iter().enumerate() {
        let old = edit
            .get("old_text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Edit {} missing old_text", index + 1))?;
        let new = edit
            .get("new_text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Edit {} missing new_text", index + 1))?;
        let unescape = edit
            .get("unescape")
            .or_else(|| arguments.get("unescape"))
            .and_then(super::value_as_bool)
            .unwrap_or(false);
        let old = if unescape {
            unescape_backslash_sequences(old)
        } else {
            old.to_string()
        };
        let new = if unescape {
            unescape_backslash_sequences(new)
        } else {
            new.to_string()
        };
        anyhow::ensure!(
            !old.is_empty(),
            "Edit {} old_text must not be empty",
            index + 1
        );
        let count = edit
            .get("count")
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(1);
        anyhow::ensure!(
            count == -1 || count > 0,
            "count must be -1 or a positive integer"
        );
        let matches = content.matches(&old).count();
        anyhow::ensure!(
            matches > 0,
            "Edit {} text not found; no edits were written",
            index + 1
        );
        anyhow::ensure!(count != 1 || matches == 1, "Edit {} multiple matches found — be more specific ({} occurrences). Use count=-1 for all.", index + 1, matches);
        let n = if count == -1 {
            matches
        } else {
            usize::try_from(count).unwrap_or(usize::MAX).min(matches)
        };
        let size = content
            .len()
            .saturating_sub(old.len().saturating_mul(n))
            .saturating_add(new.len().saturating_mul(n));
        anyhow::ensure!(
            size <= config.security.max_file_size,
            "Edited file exceeds max_file_size; no edits were written"
        );
        changed_line = content[..content.find(&old).unwrap()]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        content = content.replacen(&old, &new, n);
        replaced += n;
    }
    atomic_write(&path, content.as_bytes()).await?;
    let start = changed_line.saturating_sub(4);
    let mut context = String::new();
    for (index, line) in content.lines().enumerate().skip(start).take(8) {
        let line = format!("{:4}| {}\n", index + 1, line);
        context.push_str(super::text::byte_prefix(
            &line,
            8000usize.saturating_sub(context.len()),
        ));
        if context.len() >= 7996 {
            break;
        }
    }
    Ok(write_result(
        &path,
        content.as_bytes(),
        format!(
            "Applied {} edit(s), replaced {} occurrence(s) in {} ({} bytes)\n{}",
            edits.len(),
            replaced,
            path.display(),
            content.len(),
            context
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortalConfig;

    fn test_config() -> PortalConfig {
        let mut config = PortalConfig::default();
        config.security.workspace_root = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\workspace")
        } else {
            std::path::PathBuf::from("/workspace")
        };
        config
    }

    fn expected_path(path: &str) -> PathBuf {
        test_config().security.workspace_root.join(path)
    }

    #[test]
    fn test_resolve_relative_path() {
        let config = test_config();
        let result = resolve_path_logical(&config, "hello.txt").unwrap();
        assert_eq!(result, expected_path("hello.txt"));
    }

    #[test]
    fn test_resolve_nested_path() {
        let config = test_config();
        let result = resolve_path_logical(&config, "subdir/file.md").unwrap();
        assert_eq!(result, expected_path("subdir/file.md"));
    }

    #[test]
    fn test_resolve_dot_path() {
        let config = test_config();
        let result = resolve_path_logical(&config, "./hello.txt").unwrap();
        assert_eq!(result, expected_path("hello.txt"));
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
        let outside = if cfg!(windows) {
            r"C:\outside\file.txt"
        } else {
            "/etc/passwd"
        };
        assert!(resolve_path_logical(&config, outside).is_err());
    }

    #[test]
    fn test_allow_absolute_inside() {
        let config = test_config();
        let inside = expected_path("file.txt");
        let result = resolve_path_logical(&config, inside.to_str().unwrap()).unwrap();
        assert_eq!(result, inside);
    }

    #[test]
    fn test_reject_traversal_escape() {
        let config = test_config();
        assert!(resolve_path_logical(&config, "a/b/c/../../../../etc/passwd").is_err());
    }

    #[test]
    fn test_reject_unprepared_relative_workspace_root() {
        let mut config = PortalConfig::default();
        config.security.workspace_root = PathBuf::from(".");

        let err = resolve_path_logical(&config, "hello.txt").unwrap_err();
        assert!(err.to_string().contains("workspace root is not prepared"));
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
            err.to_string().contains("outside workspace")
                || err.to_string().contains("resolved to"),
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
            err.to_string().contains("outside workspace")
                || err.to_string().contains("resolved to"),
            "{}",
            err
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_newline() {
        assert_eq!(
            unescape_backslash_sequences("hello\\nworld"),
            "hello\nworld"
        );
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
        assert_eq!(
            unescape_backslash_sequences("plain text 123"),
            "plain text 123"
        );
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

#[cfg(test)]
mod harness_tests {
    use super::*;
    struct Workspace(PathBuf);
    impl Workspace {
        fn new() -> (Self, PortalConfig) {
            let path =
                std::env::temp_dir().join(format!("portal-harness-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            let path = path.canonicalize().unwrap();
            let mut config = PortalConfig::default();
            config.security.workspace_root = path.clone();
            (Self(path), config)
        }
    }
    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn partial_read_obeys_budget_and_handles_large_offsets() {
        let (dir, config) = Workspace::new();
        std::fs::write(dir.0.join("large.html"), "中🙂".repeat(20_000)).unwrap();
        let result = read(
            &config,
            serde_json::json!({"path":"large.html","offset":0,"limit":1000,"max_bytes":1024}),
        )
        .await
        .unwrap();
        assert_eq!(result["truncated"], true);
        assert!(result["content"][0]["text"].as_str().unwrap().len() < 1300);
        assert_eq!(result["sha256"].as_str().unwrap().len(), 64);
        read(
            &config,
            serde_json::json!({"path":"large.html","offset":u64::MAX,"limit":u64::MAX}),
        )
        .await
        .unwrap();
        std::fs::write(dir.0.join("blank.txt"), "\n\nlast").unwrap();
        let result = read(
            &config,
            serde_json::json!({"path":"blank.txt","offset":0,"limit":3}),
        )
        .await
        .unwrap();
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("\n\n\nlast"));
    }

    #[tokio::test]
    async fn svg_reads_obey_text_budget_and_support_revision_edits() {
        let (dir, config) = Workspace::new();
        let svg = format!("<svg>\n{}\n</svg>", "中".repeat(10_000));
        std::fs::write(dir.0.join("chart.svg"), &svg).unwrap();
        let result = read(&config, serde_json::json!({"path":"chart.svg","max_bytes":512})).await.unwrap();
        assert_eq!(result["truncated"], true);
        assert!(result["content"][0]["text"].as_str().unwrap().len() < 800);
        assert_eq!(result["sha256"], checksum(svg.as_bytes()));
        let partial = read(&config, serde_json::json!({"path":"chart.svg","offset":2,"limit":1,"max_bytes":512})).await.unwrap();
        assert!(partial["content"][0]["text"].as_str().unwrap().ends_with("</svg>"));
    }

    #[tokio::test]
    async fn html_is_created_then_edited_in_one_call_with_a_preview_link() {
        let (dir, config) = Workspace::new();
        let created = write(
            &config,
            serde_json::json!({"path":"page.html","content":"<h1>Old</h1><p>Text</p>"}),
        )
        .await
        .unwrap();
        let result = edit(&config, serde_json::json!({"path":"page.html","expected_sha256":created["sha256"],"edits":[{"old_text":"Old","new_text":"New"},{"old_text":"Text","new_text":"Updated"}]})).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join("page.html")).unwrap(),
            "<h1>New</h1><p>Updated</p>"
        );
        assert!(result["preview_link"].as_str().unwrap().contains("file://"));
        assert_eq!(result["sha256"], checksum(b"<h1>New</h1><p>Updated</p>"));
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("   1|"));
    }

    #[tokio::test]
    async fn invalid_batch_and_stale_revision_never_partially_write() {
        let (dir, config) = Workspace::new();
        std::fs::write(dir.0.join("page.html"), "original").unwrap();
        let result = edit(&config, serde_json::json!({"path":"page.html","edits":[{"old_text":"original","new_text":"changed"},{"old_text":"missing","new_text":"oops"}]})).await;
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("page.html")).unwrap(),
            "original"
        );
        assert!(edit(&config, serde_json::json!({"path":"page.html","old_text":"original","new_text":"changed","expected_sha256":"stale"})).await.is_err());
        assert!(edit(
            &config,
            serde_json::json!({"path":"page.html","old_text":"","new_text":"oops"})
        )
        .await
        .is_err());
        assert!(edit(&config, serde_json::json!({"path":"page.html","old_text":"original","new_text":"oops","count":0})).await.is_err());
        assert!(write(
            &config,
            serde_json::json!({"path":"page.html","content":"oops","expected_sha256":"stale"})
        )
        .await
        .is_err());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("page.html")).unwrap(),
            "original"
        );
    }

    #[tokio::test]
    async fn concurrent_edits_with_the_same_revision_have_one_winner() {
        let (dir, config) = Workspace::new();
        std::fs::write(dir.0.join("page.html"), "old").unwrap();
        let args = serde_json::json!({"path":"page.html","old_text":"old","new_text":"new","expected_sha256":checksum(b"old")});
        let (a, b) = tokio::join!(edit(&config, args.clone()), edit(&config, args));
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("page.html")).unwrap(),
            "new"
        );
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn deleting_first_line_or_all_content_does_not_panic() {
        let (dir, config) = Workspace::new();
        std::fs::write(dir.0.join("page.html"), "remove").unwrap();
        edit(
            &config,
            serde_json::json!({"path":"page.html","old_text":"remove","new_text":""}),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(dir.0.join("page.html")).unwrap(), b"");
    }
}
