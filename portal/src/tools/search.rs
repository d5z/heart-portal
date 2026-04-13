//! Workspace search — recursive grep under the portal workspace root.

use crate::config::PortalConfig;
use crate::tools::file::resolve_path_logical;
use anyhow::Result;
use regex::Regex;
use serde_json::Value;
use std::path::Path;
use tracing::debug;
use walkdir::WalkDir;

/// Search recursively under `workspace_root` for lines matching `pattern` (Rust regex syntax).
pub async fn search(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let pattern = arguments
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'pattern' argument"))?;

    let max_matches = arguments
        .get("max_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(200)
        .min(2000) as usize;

    let path_filter = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let root = resolve_path_logical(config, path_filter)?;
    if !root.starts_with(&config.security.workspace_root) {
        anyhow::bail!("Search path outside workspace");
    }
    if !root.exists() {
        anyhow::bail!("Path does not exist: {}", path_filter);
    }

    let re = Regex::new(pattern)
        .map_err(|e| anyhow::anyhow!("Invalid regex: {}", e))?;

    let workspace = config.security.workspace_root.clone();
    let max_file = config.security.max_file_size;

    debug!(
        "portal_search: pattern={:?} under {}",
        pattern,
        root.display()
    );

    let matches = tokio::task::spawn_blocking(move || {
        grep_workspace(&workspace, &root, &re, max_matches, max_file)
    })
    .await
    .map_err(|e| anyhow::anyhow!("Search task failed: {}", e))??;

    let text = serde_json::to_string(&matches)?;
    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "match_count": matches.len()
    }))
}

#[derive(serde::Serialize)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
}

fn grep_workspace(
    workspace_root: &Path,
    search_root: &Path,
    re: &Regex,
    max_matches: usize,
    max_file_bytes: usize,
) -> Result<Vec<GrepMatch>> {
    let mut out = Vec::new();

    for entry in WalkDir::new(search_root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if out.len() >= max_matches {
            break;
        }

        let path = entry.path();
        if path.is_dir() {
            continue;
        }

        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > max_file_bytes as u64 {
            continue;
        }

        let rel = match path.strip_prefix(workspace_root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        if bytes.iter().any(|&b| b == 0) {
            continue;
        }

        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };

        for (i, line) in text.lines().enumerate() {
            if out.len() >= max_matches {
                break;
            }
            if re.is_match(line) {
                out.push(GrepMatch {
                    path: rel.clone(),
                    line: i + 1,
                    text: line.chars().take(2000).collect(),
                });
            }
        }
    }

    Ok(out)
}
