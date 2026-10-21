//! Web fetch tool — retrieve URL content.

use anyhow::Result;
use serde_json::Value;
use std::net::IpAddr;
use tracing::debug;

/// Returns true if `url` may be fetched (http/https only; blocks common SSRF targets).
pub fn is_safe_url(url: &str) -> bool {
    let url = url.trim();
    if url.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:")) {
        return false;
    }
    let rest = if url
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        &url[8..]
    } else if url
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
    {
        &url[7..]
    } else {
        return false;
    };

    let end = rest
        .find(['/', '?', '#'])
        .unwrap_or(rest.len());
    let authority = &rest[..end];
    let auth_after_at = authority.rsplit('@').next().unwrap_or("");
    let host = if auth_after_at.starts_with('[') {
        let end = match auth_after_at.find(']') {
            Some(e) => e,
            None => return false,
        };
        &auth_after_at[1..end]
    } else if let Some(colon) = auth_after_at.rfind(':') {
        let after = &auth_after_at[colon + 1..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            &auth_after_at[..colon]
        } else {
            auth_after_at
        }
    } else {
        auth_after_at
    };

    host_is_safe(host)
}

fn host_is_safe(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return false;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip_is_safe(ip);
    }
    true
}

fn ip_is_safe(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() || v4.is_broadcast() {
                return false;
            }
            !(v4.is_loopback() || v4.is_private() || v4.is_link_local())
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return false;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_safe(IpAddr::V4(v4));
            }
            let s = v6.segments();
            // Unique local (fc00::/7) and link-local unicast (fe80::/10)
            if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 {
                return false;
            }
            true
        }
    }
}

pub async fn fetch(arguments: Value) -> Result<Value> {
    #[cfg(target_os = "android")]
    return android_fetch(arguments).await;
    #[cfg(not(target_os = "android"))]
    desktop_fetch(arguments).await
}

#[cfg(not(target_os = "android"))]
async fn desktop_fetch(arguments: Value) -> Result<Value> {
    let url = arguments.get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;

    if !is_safe_url(url) {
        anyhow::bail!("URL is not allowed (blocked for SSRF protection)");
    }

    // Keep the existing parameter name and byte budget for compatibility.
    let max_bytes = arguments.get("max_chars")
        .and_then(super::value_as_u64)
        .unwrap_or(50_000) as usize;

    debug!("web_fetch: {} (max_bytes: {})", url, max_bytes);

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
        anyhow::bail!("Fetch failed (exit {}): {}", output.status.code().unwrap_or_else(|| {
            tracing::debug!("Process terminated by signal, no exit code available");
            -1
        }), stderr);
    }

    let body = String::from_utf8_lossy(&output.stdout);
    let truncated = if body.len() > max_bytes {
        let prefix = super::text::byte_prefix(&body, max_bytes);
        format!("{}...\n(truncated at {} bytes)", prefix, prefix.len())
    } else {
        body.to_string()
    };

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": truncated }]
    }))
}

/// Android does not ship curl. Bound bytes and redirects, and pin validated DNS
/// answers so private-address redirects / DNS rebinding cannot bypass the URL check.
#[cfg(target_os = "android")]
async fn android_fetch(arguments: Value) -> Result<Value> {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let input = arguments.get("url").and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;
        let mut url = url::Url::parse(input)?;
        let budget = arguments.get("max_chars").and_then(super::value_as_u64)
            .unwrap_or(50_000).clamp(1, 1_000_000) as usize;
        for _ in 0..6 {
            anyhow::ensure!(matches!(url.scheme(), "http" | "https") && url.username().is_empty() && url.password().is_none(), "Unsupported web URL");
            let host = url.host_str().ok_or_else(|| anyhow::anyhow!("Missing host"))?;
            let addresses: Vec<_> = tokio::net::lookup_host((host, url.port_or_known_default().unwrap())).await?.collect();
            anyhow::ensure!(!addresses.is_empty() && addresses.iter().all(|address| ip_is_safe(address.ip())), "URL is not allowed (blocked for SSRF protection)");
            let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
                .no_proxy().resolve_to_addrs(host, &addresses)
                .timeout(std::time::Duration::from_secs(12)).build()?;
            let mut response = client.get(url.clone()).header("User-Agent", "heart-portal-android/0.9.2").send().await?;
            if response.status().is_redirection() {
                let location = response.headers().get(reqwest::header::LOCATION)
                    .ok_or_else(|| anyhow::anyhow!("Redirect without location"))?.to_str()?;
                url = url.join(location)?;
                continue;
            }
            response = response.error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                let remaining = (budget + 1).saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
                if bytes.len() > budget { break; }
            }
            let text = String::from_utf8_lossy(&bytes);
            let text = if bytes.len() > budget {
                format!("{}...\n(truncated at {} bytes)", super::text::byte_prefix(&text, budget), budget)
            } else { text.to_string() };
            return Ok(serde_json::json!({"content": [{"type": "text", "text": text}]}));
        }
        anyhow::bail!("Too many redirects")
    }).await.map_err(|_| anyhow::anyhow!("web_fetch timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_multibyte_scheme_without_panicking() {
        for url in ["中文🙂", "https中://example.com", "http中://example.com"] {
            assert!(!is_safe_url(url));
        }
    }

    #[test]
    fn allows_public_https() {
        assert!(is_safe_url("https://example.com/path"));
    }

    #[test]
    fn rejects_file_scheme() {
        assert!(!is_safe_url("file:///etc/passwd"));
    }

    #[test]
    fn rejects_loopback() {
        assert!(!is_safe_url("http://127.0.0.1/"));
        assert!(!is_safe_url("http://localhost/"));
        assert!(!is_safe_url("http://[::1]/"));
    }

    #[test]
    fn rejects_private_and_link_local() {
        assert!(!is_safe_url("http://10.0.0.1/"));
        assert!(!is_safe_url("http://172.20.1.1/"));
        assert!(!is_safe_url("http://192.168.0.1/"));
        assert!(!is_safe_url("http://169.254.169.254/latest/meta-data/"));
    }
}
