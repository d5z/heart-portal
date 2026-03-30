//! Web search tool — Google Custom Search API.

use anyhow::Result;
use serde_json::Value;
use tracing::debug;

/// Google Custom Search API endpoint
const GOOGLE_CSE_ENDPOINT: &str = "https://www.googleapis.com/customsearch/v1";

/// Simple percent-encoding for URL query parameters
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

pub async fn search(arguments: Value) -> Result<Value> {
    let query = arguments.get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'query' argument"))?;

    let count = arguments.get("count")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .min(10) as usize;

    // Read API credentials from environment
    let api_key = std::env::var("GOOGLE_API_KEY")
        .map_err(|_| anyhow::anyhow!("GOOGLE_API_KEY not set"))?;
    let cx = std::env::var("GOOGLE_SEARCH_CX")
        .map_err(|_| anyhow::anyhow!("GOOGLE_SEARCH_CX not set"))?;

    debug!("web_search: '{}' (count: {})", query, count);

    // Build URL
    let url = format!(
        "{}?key={}&cx={}&q={}&num={}",
        GOOGLE_CSE_ENDPOINT,
        url_encode(&api_key),
        url_encode(&cx),
        url_encode(query),
        count
    );

    // Fetch via curl (consistent with web_fetch, no extra deps)
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("curl")
            .args(["-sfL", "--max-time", "10", "-A", "heart-portal/0.1", &url])
            .output()
    ).await
        .map_err(|_| anyhow::anyhow!("web_search timed out"))?
        .map_err(|e| anyhow::anyhow!("Failed to search: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Search failed (exit {}): {}", output.status.code().unwrap_or(-1), stderr);
    }

    let body: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse search response: {}", e))?;

    // Extract results
    let items = body.get("items").and_then(|v| v.as_array());
    let results: Vec<Value> = match items {
        Some(items) => items.iter().take(count).map(|item| {
            serde_json::json!({
                "title": item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "url": item.get("link").and_then(|v| v.as_str()).unwrap_or(""),
                "snippet": item.get("snippet").and_then(|v| v.as_str()).unwrap_or(""),
            })
        }).collect(),
        None => vec![],
    };

    let text = if results.is_empty() {
        "No results found.".to_string()
    } else {
        results.iter().enumerate().map(|(i, r)| {
            format!("{}. {}\n   {}\n   {}",
                i + 1,
                r["title"].as_str().unwrap_or(""),
                r["url"].as_str().unwrap_or(""),
                r["snippet"].as_str().unwrap_or(""),
            )
        }).collect::<Vec<_>>().join("\n\n")
    };

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "results": results,
        "total": results.len()
    }))
}
