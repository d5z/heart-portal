//! DuckDuckGo HTML search — no API key.

use anyhow::Result;
use reqwest::Url;
use scraper::{Html, Selector};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Serialize)]
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

fn resolve_result_url(href: &str) -> String {
    let full = if href.starts_with("//") {
        format!("https:{}", href)
    } else if href.starts_with('/') && !href.starts_with("//") {
        format!("https://html.duckduckgo.com{}", href)
    } else {
        href.to_string()
    };
    if let Ok(u) = Url::parse(&full) {
        for (k, v) in u.query_pairs() {
            if k == "uddg" {
                return v.into_owned();
            }
        }
    }
    href.to_string()
}

fn parse_hits(html: &str, limit: usize) -> Vec<SearchHit> {
    let document = Html::parse_document(html);
    let result_sel = Selector::parse(".result").ok();
    let title_a_sel = Selector::parse("a.result__a").ok();
    let snippet_a_sel = Selector::parse("a.result__snippet").ok();
    let snippet_div_sel = Selector::parse(".result__snippet").ok();

    let mut out = Vec::new();

    let Some(result_sel) = result_sel else {
        return out;
    };
    let Some(title_a_sel) = title_a_sel else {
        return out;
    };

    for result in document.select(&result_sel) {
        if out.len() >= limit {
            break;
        }
        let title_el = result.select(&title_a_sel).next();
        let Some(title_el) = title_el else {
            continue;
        };
        let href = title_el.attr("href").unwrap_or("");
        let title = title_el.text().collect::<Vec<_>>().join("").trim().to_string();
        if title.is_empty() && href.is_empty() {
            continue;
        }

        let mut snippet = String::new();
        if let Some(ref snippet_a_sel) = snippet_a_sel {
            if let Some(sn) = result.select(snippet_a_sel).next() {
                snippet = sn.text().collect::<Vec<_>>().join("").trim().to_string();
            }
        }
        if snippet.is_empty() {
            if let Some(ref snippet_div_sel) = snippet_div_sel {
                if let Some(sn) = result.select(snippet_div_sel).next() {
                    snippet = sn.text().collect::<Vec<_>>().join("").trim().to_string();
                }
            }
        }

        let url = resolve_result_url(href);
        if url.is_empty() {
            continue;
        }
        out.push(SearchHit { title, url, snippet });
    }

    out
}

pub async fn search(arguments: Value) -> Result<Value> {
    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'query' argument"))?
        .trim();
    if query.is_empty() {
        anyhow::bail!("'query' must not be empty");
    }

    let count = arguments
        .get("count")
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .filter(|&n| n > 0)
        .map(|n| (n as u8).clamp(1, 10))
        .unwrap_or(5);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (compatible; heart-portal/0.1; +https://example.invalid)")
        .build()
        .map_err(|e| anyhow::anyhow!("HTTP client: {}", e))?;

    let url = Url::parse_with_params(
        "https://html.duckduckgo.com/html/",
        &[("q", query)],
    )
    .map_err(|e| anyhow::anyhow!("Invalid query URL: {}", e))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                anyhow::anyhow!("Search request timed out after 10s")
            } else {
                anyhow::anyhow!("Search request failed: {}", e)
            }
        })?;

    if !resp.status().is_success() {
        anyhow::bail!("Search returned HTTP {}", resp.status());
    }

    let body = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read search response: {}", e))?;

    let hits = parse_hits(&body, count as usize);
    let json = serde_json::to_string(&hits)
        .map_err(|e| anyhow::anyhow!("Failed to serialize results: {}", e))?;

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": json }]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_ddg_html() {
        let html = r#"<!DOCTYPE html><html><body>
<div class="result results_links results_links_deep web-result">
  <div class="links_main links_deep result__body">
    <h2 class="result__title">
      <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F">Example Domain</a>
    </h2>
    <a class="result__snippet">This domain is for use in examples.</a>
  </div>
</div>
</body></html>"#;
        let hits = parse_hits(html, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Example Domain");
        assert_eq!(hits[0].url, "https://example.com/");
        assert!(hits[0].snippet.contains("examples"));
    }
}
