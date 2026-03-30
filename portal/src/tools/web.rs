//! Web fetch tool — retrieve URL content.

use anyhow::Result;
use serde_json::Value;
use tracing::debug;

pub async fn fetch(arguments: Value) -> Result<Value> {
    let url = arguments.get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;

    let max_chars = arguments.get("max_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(50_000) as usize;

    debug!("web_fetch: {} (max_chars: {})", url, max_chars);

    // Use a simple HTTP client via command (keeps binary small, no reqwest dep)
    // For v0.2+, consider adding reqwest as optional dependency
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("curl")
            .args(["-sfL", "--max-time", "10", "-A", "heart-portal/0.1", url])
            .output()
    ).await
        .map_err(|_| anyhow::anyhow!("web_fetch timed out"))?
        .map_err(|e| anyhow::anyhow!("Failed to fetch: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Fetch failed (exit {}): {}", output.status.code().unwrap_or(-1), stderr);
    }

    let body = String::from_utf8_lossy(&output.stdout);
    let truncated = if body.len() > max_chars {
        format!("{}...\n(truncated at {} chars)", &body[..max_chars], max_chars)
    } else {
        body.to_string()
    };

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": truncated }]
    }))
}
