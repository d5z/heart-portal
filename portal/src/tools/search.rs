//! Workspace search — recursive grep under the portal workspace root.

use crate::config::PortalConfig;
use crate::tools::file::{canonical_workspace_root, resolve_existing_path};
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
        .and_then(super::value_as_u64)
        .unwrap_or(200)
        .clamp(1, 2000) as usize;

    let path_filter = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");

    let root = resolve_existing_path(config, path_filter)?;
    let include_ignored = arguments
        .get("include_ignored")
        .and_then(super::value_as_bool)
        .unwrap_or(false);

    let re = Regex::new(pattern).map_err(|e| anyhow::anyhow!("Invalid regex: {}", e))?;

    // Keep both paths in canonical form. This is especially important on
    // Windows, where canonicalization may use the verbatim-path prefix.
    let workspace = canonical_workspace_root(config)?;
    let max_file = config.security.max_file_size;

    debug!(
        "portal_search: pattern={:?} under {}",
        pattern,
        root.display()
    );

    let matches = tokio::task::spawn_blocking(move || {
        grep_workspace(
            &workspace,
            &root,
            &re,
            max_matches,
            max_file,
            include_ignored,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("Search task failed: {}", e))??;

    let text = serde_json::to_string(&matches.items)?;
    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "match_count": matches.items.len(), "truncated": matches.truncated,
        "files_scanned": matches.files_scanned, "skipped_directories": matches.skipped_directories
    }))
}

#[derive(serde::Serialize)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
}

#[derive(Default)]
struct SearchResults {
    items: Vec<GrepMatch>,
    truncated: bool,
    files_scanned: usize,
    skipped_directories: usize,
}
fn ignored_directory(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".venv"
            | "venv"
            | "__pycache__"
    )
}

fn grep_workspace(
    workspace_root: &Path,
    search_root: &Path,
    re: &Regex,
    max_matches: usize,
    max_file_bytes: usize,
    include_ignored: bool,
) -> Result<SearchResults> {
    let mut result = SearchResults::default();
    let mut out = Vec::new();
    let mut output_bytes = 2usize;
    let mut skipped_directories = 0;

    for entry in WalkDir::new(search_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let skip = !include_ignored
                && entry.depth() > 0
                && entry.file_type().is_dir()
                && ignored_directory(&entry.file_name().to_string_lossy());
            if skip {
                skipped_directories += 1;
            }
            !skip
        })
        .filter_map(|e| e.ok())
    {
        if out.len() >= max_matches {
            result.truncated = true;
            break;
        }

        let path = entry.path();
        // Never follow a nested symlink/reparse point. `follow_links(false)`
        // prevents directory traversal, while this also protects file reads.
        if entry.file_type().is_symlink() || entry.file_type().is_dir() {
            continue;
        }

        let canonical = match path.canonicalize() {
            Ok(path) if path.starts_with(workspace_root) => path,
            _ => continue,
        };

        let meta = match std::fs::metadata(&canonical) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.len() > max_file_bytes as u64 {
            continue;
        }

        let rel = match canonical.strip_prefix(workspace_root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        result.files_scanned += 1;
        let bytes = match crate::bounded_file::read(&canonical, max_file_bytes) {
            Ok(b) => b,
            Err(_) => continue,
        };

        if bytes.contains(&0) {
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
                let found = GrepMatch {
                    path: rel.clone(),
                    line: i + 1,
                    text: super::text::byte_prefix(line, 2000).to_string(),
                };
                output_bytes += serde_json::to_vec(&found)?.len() + 1;
                if output_bytes > 100_000 {
                    result.truncated = true;
                    break;
                }
                out.push(found);
            }
        }
        if result.truncated {
            break;
        }
    }

    result.items = out;
    result.truncated |= result.items.len() >= max_matches;
    result.skipped_directories = skipped_directories;
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn nested_file_symlink_cannot_escape_workspace() {
        let temp = std::env::temp_dir().join(format!(
            "portal-search-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace = temp.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("inside.txt"), "search-marker inside").unwrap();

        let outside = temp.join("outside.txt");
        std::fs::write(&outside, "search-marker outside").unwrap();
        symlink(&outside, workspace.join("leak.txt")).unwrap();

        let workspace = workspace.canonicalize().unwrap();
        let re = Regex::new("search-marker").unwrap();
        let matches = grep_workspace(&workspace, &workspace, &re, 10, 1024, false).unwrap();

        assert_eq!(matches.items.len(), 1);
        assert_eq!(matches.items[0].path, "inside.txt");
        assert_eq!(matches.items[0].text, "search-marker inside");

        std::fs::remove_dir_all(&temp).unwrap();
    }
}

#[cfg(test)]
mod efficiency_tests {
    use super::*;
    #[test]
    fn excludes_dependencies_but_allows_explicit_opt_in_and_root() {
        let dir =
            std::env::temp_dir().join(format!("portal-search-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("node_modules/package")).unwrap();
        std::fs::write(dir.join("index.html"), "marker").unwrap();
        for i in 0..100 {
            std::fs::write(dir.join(format!("node_modules/package/{i}.js")), "marker").unwrap();
        }
        let root = dir.canonicalize().unwrap();
        let pattern = Regex::new("marker").unwrap();
        let normal = grep_workspace(&root, &root, &pattern, 200, 1024, false).unwrap();
        assert_eq!(normal.files_scanned, 1);
        assert_eq!(normal.items.len(), 1);
        assert_eq!(normal.skipped_directories, 1);
        let all = grep_workspace(&root, &root, &pattern, 200, 1024, true).unwrap();
        assert_eq!(all.files_scanned, 101);
        assert_eq!(all.items.len(), 101);
        let explicit = grep_workspace(
            &root,
            &root.join("node_modules"),
            &pattern,
            200,
            1024,
            false,
        )
        .unwrap();
        assert_eq!(explicit.items.len(), 100);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn limits_serialized_output_even_when_match_count_is_large() {
        let dir =
            std::env::temp_dir().join(format!("portal-search-output-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("large.txt"),
            format!("{}\n", "中".repeat(1000)).repeat(1000),
        )
        .unwrap();
        let root = dir.canonicalize().unwrap();
        let result = grep_workspace(
            &root,
            &root,
            &Regex::new("中").unwrap(),
            2000,
            4_000_000,
            false,
        )
        .unwrap();
        assert!(result.truncated);
        assert!(serde_json::to_vec(&result.items).unwrap().len() <= 100_000);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
