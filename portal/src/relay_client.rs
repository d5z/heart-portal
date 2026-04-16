//! Connect mode — Portal dials Hearth relay via WebSocket (reverse path for NAT traversal).
//! The relay endpoint is at `wss://host/_relay`, derived from the Loom URL.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{info, warn};

use crate::tools::ToolHost;

/// Parse a Loom link: `https://host[:port]/being/?token=...` → (host_with_port, being_id, token).
pub fn parse_loom_link(s: &str) -> Result<(String, String, String)> {
    let s = s.trim();
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .ok_or_else(|| anyhow::anyhow!("Loom link must start with http:// or https://"))?;
    let (host_port, path_and_query) = rest
        .split_once('/')
        .unwrap_or((rest, ""));
    let host = host_port.to_string();
    let (path_part, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let being_id = path_part
        .trim_matches('/')
        .split('/')
        .find(|x| !x.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing being id in URL path"))?
        .to_string();
    if being_id.is_empty() {
        anyhow::bail!("empty being id in URL path");
    }
    let mut token = None;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "token" {
                token = Some(v.to_string());
                break;
            }
        }
    }
    let token = token.ok_or_else(|| anyhow::anyhow!("missing token query parameter"))?;
    Ok((host, being_id, token))
}

/// Derive WebSocket relay URL from Loom host.
/// `echo.beings.town` → `wss://echo.beings.town/_relay`
/// `localhost:3100` (http) → `ws://localhost:3100/_relay`  (for local testing)
fn derive_relay_url(_loom_link: &str, host: &str) -> String {
    let is_localhost = host.starts_with("localhost") || host.starts_with("127.");
    let scheme = if is_localhost { "ws" } else { "wss" };
    format!("{scheme}://{host}/_relay")
}

/// Run connect mode with automatic reconnect (exponential backoff, max 60s).
pub async fn connect_and_serve(
    loom_link: &str,
    tool_host: &ToolHost,
    portal_name: &str,
) {
    let (host, being_id, token) = match parse_loom_link(loom_link) {
        Ok(x) => x,
        Err(e) => {
            warn!("invalid Loom link: {e:#}");
            return;
        }
    };
    let relay_url = derive_relay_url(loom_link, &host);
    info!(
        "Portal connect mode: relay {} (being_id={}, host={})",
        relay_url, being_id, host
    );

    let mut backoff = Duration::from_secs(5);
    loop {
        match run_one_session(&relay_url, &being_id, &token, tool_host, portal_name).await {
            Ok(()) => {
                backoff = Duration::from_secs(5);
                info!("relay session ended cleanly; reconnecting in {:?}", backoff);
            }
            Err(e) => {
                warn!("relay session error: {e:#}; retry in {:?}", backoff);
            }
        }
        tokio::time::sleep(backoff).await;
        let next_secs = (backoff.as_secs().saturating_mul(2)).min(60).max(5);
        backoff = Duration::from_secs(next_secs);
    }
}

async fn run_one_session(
    relay_url: &str,
    being_id: &str,
    token: &str,
    tool_host: &ToolHost,
    portal_name: &str,
) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(relay_url)
        .await
        .with_context(|| format!("WebSocket connect to relay {relay_url}"))?;

    // Send handshake
    let handshake = serde_json::json!({
        "being_id": being_id,
        "loom_token": token,
    });
    ws.send(Message::Text(handshake.to_string().into())).await?;

    // Read handshake response
    let resp = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => t,
        Ok(Some(Ok(_))) => anyhow::bail!("unexpected non-text response"),
        Ok(Some(Err(e))) => anyhow::bail!("ws recv error: {e}"),
        Ok(None) => anyhow::bail!("ws closed before handshake response"),
        Err(_) => anyhow::bail!("handshake response timeout"),
    };

    let v: serde_json::Value = serde_json::from_str(resp.as_str()).context("relay handshake JSON")?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        anyhow::bail!("relay rejected handshake: {}", resp.as_str());
    }

    info!("Portal relay handshake OK — starting MCP server on WebSocket bridge");

    // Bridge WebSocket ↔ MCP TCP connection.
    // Portal's handle_connection expects a TcpStream-like AsyncRead+AsyncWrite.
    // We create a duplex pipe: one end for the portal MCP handler, other end bridged to WebSocket.
    let (portal_stream, bridge_stream) = tokio::io::duplex(65536);

    let (mut ws_write, mut ws_read) = ws.split();
    let (bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    let mut bridge_reader = tokio::io::BufReader::new(bridge_read);

    // Task 1: WS → bridge (relay sends MCP requests via WS, portal reads from bridge)
    let ws_to_bridge = tokio::spawn(async move {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    let mut data = t.as_str().as_bytes().to_vec();
                    data.push(b'\n');
                    if tokio::io::AsyncWriteExt::write_all(&mut bridge_write, &data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    // Task 2: bridge → WS (portal writes MCP responses to bridge, we send via WS)
    let bridge_to_ws = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match tokio::io::AsyncBufReadExt::read_line(&mut bridge_reader, &mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        if ws_write.send(Message::Text(trimmed.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Task 3: Portal MCP handler on the duplex stream
    crate::handle_connection(portal_stream, tool_host, portal_name, None).await?;

    // When handle_connection ends, abort bridges
    ws_to_bridge.abort();
    bridge_to_ws.abort();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_standard_loom_link() {
        let (host, being, token) = parse_loom_link(
            "https://echo.beings.town/hex/?token=abc123"
        ).unwrap();
        assert_eq!(host, "echo.beings.town");
        assert_eq!(being, "hex");
        assert_eq!(token, "abc123");
    }

    #[test]
    fn parse_loom_link_with_port() {
        let (host, being, token) = parse_loom_link(
            "https://echo.beings.town:8443/hex/?token=abc123"
        ).unwrap();
        assert_eq!(host, "echo.beings.town:8443");
        assert_eq!(being, "hex");
        assert_eq!(token, "abc123");
    }

    #[test]
    fn parse_loom_link_no_trailing_slash() {
        let (host, being, token) = parse_loom_link(
            "https://echo.beings.town/hex?token=abc123"
        ).unwrap();
        assert_eq!(host, "echo.beings.town");
        assert_eq!(being, "hex");
        assert_eq!(token, "abc123");
    }

    #[test]
    fn parse_loom_link_missing_token() {
        assert!(parse_loom_link("https://echo.beings.town/hex/").is_err());
    }

    #[test]
    fn parse_loom_link_missing_being() {
        assert!(parse_loom_link("https://echo.beings.town/?token=abc").is_err());
    }

    #[test]
    fn parse_loom_link_http() {
        let (host, being, token) = parse_loom_link(
            "http://localhost:3100/alice/?token=test"
        ).unwrap();
        assert_eq!(host, "localhost:3100");
        assert_eq!(being, "alice");
        assert_eq!(token, "test");
    }

    #[test]
    fn derive_relay_url_https() {
        let url = derive_relay_url("https://echo.beings.town/alice/?token=t", "echo.beings.town");
        assert_eq!(url, "wss://echo.beings.town/_relay");
    }

    #[test]
    fn derive_relay_url_localhost() {
        let url = derive_relay_url("http://localhost:3100/alice/?token=t", "localhost:3100");
        assert_eq!(url, "ws://localhost:3100/_relay");
    }
}
