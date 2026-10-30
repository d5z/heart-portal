//! Web fetch tool — retrieve URL content.

use anyhow::Result;
use serde_json::Value;
use std::net::IpAddr;
use tracing::debug;

/// Returns true if `url` may be fetched (http/https only; blocks common SSRF targets).
pub fn is_safe_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else { return false; };
    matches!(url.scheme(), "http" | "https") && url.username().is_empty() && url.password().is_none()
        && url.host_str().is_some_and(|host| host_is_safe(host.trim_matches(['[', ']'])))
}

fn host_is_safe(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return false;
    }
    let lower = host.to_ascii_lowercase();
    if lower.trim_end_matches('.') == "localhost" || lower.trim_end_matches('.').ends_with(".localhost") {
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
            !(v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_multicast()
                || v4.octets()[0] == 0 || v4.octets()[0] >= 240
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])))
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
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
    let url = arguments.get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'url' argument"))?;

    if !is_safe_url(url) {
        anyhow::bail!("URL is not allowed (blocked for SSRF protection)");
    }

    let max_bytes = arguments.get("max_chars")
        .and_then(super::value_as_u64)
        .unwrap_or(50_000).clamp(1, 1_000_000) as usize;
    let initial = url::Url::parse(url)?;
    debug!("web_fetch: host={:?}, max_bytes={}", initial.host_str(), max_bytes);
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(15), fetch_bytes(initial, max_bytes))
        .await.map_err(|_| anyhow::anyhow!("web_fetch timed out"))??;
    let body = String::from_utf8_lossy(&bytes);
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

fn redirect_target(current: &url::Url, location: &str) -> Result<url::Url> {
    let next = current.join(location)?;
    anyhow::ensure!(is_safe_url(next.as_str()) && next.origin() == current.origin(),
        "Redirect leaves the approved origin; request the new URL separately");
    Ok(next)
}

async fn fetch_bytes(mut url: url::Url, max_bytes: usize) -> Result<Vec<u8>> {
    for _ in 0..=5 {
        let host = url.host_str().ok_or_else(|| anyhow::anyhow!("Missing URL host"))?.trim_matches(['[', ']']);
        let addresses: Vec<_> = tokio::net::lookup_host((host, url.port_or_known_default().unwrap())).await?.collect();
        anyhow::ensure!(!addresses.is_empty() && addresses.iter().all(|address| ip_is_safe(address.ip())),
            "URL resolves to a non-public address");
        // Validate and pin the same DNS answers. Do not inherit proxy credentials,
        // curl configuration, cookies or executables from the host/workspace.
        let client = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addresses).timeout(std::time::Duration::from_secs(10)).build()?;
        let mut response = client.get(url.clone()).header(reqwest::header::USER_AGENT, "heart-portal").send().await?;
        if response.status().is_redirection() {
            let location = response.headers().get(reqwest::header::LOCATION)
                .ok_or_else(|| anyhow::anyhow!("Redirect without a location"))?.to_str()?;
            url = redirect_target(&url, location)?;
            continue;
        }
        anyhow::ensure!(response.status().is_success(), "Fetch failed ({})", response.status());
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            let remaining = (max_bytes + 1).saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if bytes.len() > max_bytes { break; }
        }
        return Ok(bytes);
    }
    anyhow::bail!("Too many redirects")
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

    #[test]
    fn redirects_cannot_expand_the_approved_origin() {
        let url = url::Url::parse("https://docs.python.org/3").unwrap();
        assert_eq!(redirect_target(&url, "/3/").unwrap().as_str(), "https://docs.python.org/3/");
        for target in ["https://127.0.0.1/", "https://example.org/", "http://docs.python.org/", "https://user:pass@docs.python.org/", "file:///etc/passwd"] {
            assert!(redirect_target(&url, target).is_err());
        }
        for target in ["http://2130706433/", "http://127.1/", "http://[::ffff:127.0.0.1]/", "https://user:pass@example.org/", "http://localhost./", "http://100.64.0.1/"] {
            assert!(!is_safe_url(target), "{target}");
        }
    }
}
