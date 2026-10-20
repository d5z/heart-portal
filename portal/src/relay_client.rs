//! Connect mode — Portal dials Hearth relay via WebSocket (reverse path for NAT traversal).
//! The relay endpoint is at `wss://host/_relay`, derived from the Loom URL.
//!
//! D-077 (Portal Relay 独立化): duplex relay with explicit heartbeat.
//! D-078 (断连不立即注销工具): client treats heartbeat loss as disconnect and reconnects;
//! server-side grace lives in `hearth::portal_relay`.

use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{info, warn};
use tokio::time::MissedTickBehavior;

use crate::tools::ToolHost;

/// Portal → relay JSON handshake ping interval (D-077).
/// Reduced from 30s to 15s in v0.7.1 to shrink the no-activity window
/// on platforms (Windows) where network devices may kill idle TCP.
const HEARTBEAT_INTERVAL_SECS: u64 = 15;
/// If no relay frame is received within this window, reconnect (D-077).
const HEARTBEAT_TIMEOUT_SECS: u64 = 90;
const SCHEDULING_GAP_MULTIPLIER: u32 = 3;

#[derive(Deserialize)]
struct HandshakeResponse {
    ok: bool,
    #[serde(default)]
    relay_keepalive: Option<String>,
}

/// Build relay handshake JSON (`portal_name` identifies this Portal instance; D-077).
pub(crate) fn relay_handshake_json(being_id: &str, loom_token: &str, portal_name: &str) -> serde_json::Value {
    serde_json::json!({
        "being_id": being_id,
        "loom_token": loom_token,
        "portal_name": portal_name,
    })
}

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
    let scheme = if is_loopback_host(host) { "ws" } else { "wss" };
    format!("{scheme}://{host}/_relay")
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    // Deliberately conservative: localhost. and IPv4-mapped IPv6 addresses
    // retain TLS. Only exact localhost and native loopback IPs permit plaintext.
    url::Url::parse(&format!("http://{host}")).is_ok_and(|url| match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    })
}

/// Run connect mode with automatic reconnect (exponential backoff, 2s..30s + jitter).
pub async fn connect_and_serve(
    loom_link: &str,
    tool_host: &ToolHost,
    portal_name: &str,
) {
    let (host, being_id, token) = match parse_loom_link(loom_link) {
        Ok(x) => x,
        Err(e) => {
            tool_host.set_connection_state(crate::tools::status::ConnectionState::Invalid);
            #[cfg(windows)]
            crate::connection_status::publish("invalid");
            warn!("invalid Loom link: {e:#}");
            return;
        }
    };
    let relay_url = derive_relay_url(loom_link, &host);
    info!(
        "Portal connect mode: relay {} (being_id={}, portal_name={}, host={})",
        relay_url, being_id, portal_name, host
    );

    let mut backoff = Duration::from_secs(BACKOFF_MIN_SECS);
    loop {
        tool_host.set_connection_state(crate::tools::status::ConnectionState::Connecting);
        #[cfg(windows)]
        crate::connection_status::publish("connecting");
        let session_start = Instant::now();
        match run_one_session(&relay_url, &being_id, &token, tool_host, portal_name).await {
            Ok(()) => {
                backoff = Duration::from_secs(BACKOFF_MIN_SECS);
                info!("relay session ended cleanly; reconnecting in {:?}", backoff);
            }
            Err(e) => {
                // Ratchet fix: `run_one_session` almost always ends in Err
                // (heartbeat timeout = bail!), so resetting only on Ok left
                // long-lived Portals stuck at the max backoff forever. A
                // session that survived past HEALTHY_SESSION_SECS proves the
                // link itself was fine — treat it as a normal session end.
                if session_start.elapsed() > Duration::from_secs(HEALTHY_SESSION_SECS) {
                    backoff = Duration::from_secs(BACKOFF_MIN_SECS);
                    info!(
                        "relay session ran {}s before error, resetting backoff: {e:#}",
                        session_start.elapsed().as_secs()
                    );
                } else {
                    warn!(
                        "relay session error after {}s: {e:#}; retry in {:?}",
                        session_start.elapsed().as_secs(),
                        backoff
                    );
                }
            }
        }
        tool_host.set_connection_state(crate::tools::status::ConnectionState::Retrying);
        #[cfg(windows)]
        crate::connection_status::publish("retrying");
        tokio::time::sleep(Duration::from_millis(backoff_sleep_ms(backoff, jitter_ratio())))
            .await;
        backoff = next_backoff(backoff);
    }
}

/// Reconnect backoff floor (also the reset value).
const BACKOFF_MIN_SECS: u64 = 2;
/// Reconnect backoff ceiling.
const BACKOFF_MAX_SECS: u64 = 30;
/// A session living longer than this counts as healthy: its error is a normal
/// session end (heartbeat timeout), not a connect failure, so backoff resets.
const HEALTHY_SESSION_SECS: u64 = 60;
/// Never sleep less than this between reconnects (protects the relay from a
/// hot reconnect loop when backoff is at its floor and jitter is negative).
const BACKOFF_FLOOR_MS: u64 = 1000;

/// Pseudo-random jitter ratio in `[-0.3, 0.3)`, derived from the clock so no
/// RNG dependency is needed. Spreads reconnects when many Portals restart together.
fn jitter_ratio() -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    // 0.0..1.0 → -0.3..0.3
    ((h.finish() % 10_000) as f64 / 10_000.0) * 0.6 - 0.3
}

/// Apply `ratio` jitter to `backoff`, clamped to [`BACKOFF_FLOOR_MS`].
fn backoff_sleep_ms(backoff: Duration, ratio: f64) -> u64 {
    let base = backoff.as_millis() as f64;
    let jittered = base + base * ratio;
    if jittered < BACKOFF_FLOOR_MS as f64 {
        BACKOFF_FLOOR_MS
    } else {
        jittered as u64
    }
}

/// Double the backoff, clamped to `[BACKOFF_MIN_SECS, BACKOFF_MAX_SECS]`.
fn next_backoff(backoff: Duration) -> Duration {
    let secs = backoff
        .as_secs()
        .saturating_mul(2)
        .clamp(BACKOFF_MIN_SECS, BACKOFF_MAX_SECS);
    Duration::from_secs(secs)
}

async fn run_one_session(
    relay_url: &str,
    being_id: &str,
    token: &str,
    tool_host: &ToolHost,
    portal_name: &str,
) -> Result<()> {
    // D-077 v0.7.1: create TCP stream with keepalive to prevent
    // silent connection death on Windows / behind NATs.
    let mut ws = {
        let parsed: url::Url = relay_url.parse().context("parse relay URL")?;
        let host = parsed.host_str().unwrap_or("localhost");
        let default_port = if parsed.scheme() == "wss" { 443 } else { 80 };
        let port = parsed.port_or_known_default().unwrap_or(default_port);
        let addr = format!("{host}:{port}");
        let tcp = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::net::TcpStream::connect(&addr),
        )
            .await
            .with_context(|| format!("TCP connect to relay {addr} timed out (15s)"))?
            .with_context(|| format!("TCP connect to relay {addr}"))?;
        // TCP keepalive: 15s idle + 5s probe interval (survives NAT/firewall idle timeouts)
        let sock_ref = socket2::SockRef::from(&tcp);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(15))
            .with_interval(Duration::from_secs(5));
        // TCP_KEEPCNT (retries) is not exposed by socket2 0.5.x on windows targets;
        // OS default probe count applies (Linux/Android 9, Win10 10). 15s idle + 5s interval via with_time/with_interval.
        #[allow(unused_mut)]
        let keepalive = keepalive;
        if let Err(error) = sock_ref.set_tcp_keepalive(&keepalive) {
            warn!("relay TCP keepalive configuration failed: {error}");
        }
        let _ = sock_ref.set_nodelay(true);
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(15),
            tokio_tungstenite::client_async_tls_with_config(relay_url, tcp, Some(
                tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
                    max_message_size: Some(16 * 1024 * 1024),
                    max_frame_size: Some(16 * 1024 * 1024),
                    ..Default::default()
                }), None),
        )
            .await
            .with_context(|| format!("TLS/WS handshake to relay {relay_url} timed out (15s)"))?
            .with_context(|| format!("WebSocket connect to relay {relay_url}"))?;
        ws
    };

    let handshake = relay_handshake_json(being_id, token, portal_name);
    tokio::time::timeout(Duration::from_secs(10), ws.send(Message::Text(handshake.to_string())))
        .await.context("relay handshake send timed out")??;

    // Read handshake response
    let resp = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => t,
        Ok(Some(Ok(_))) => anyhow::bail!("unexpected non-text response"),
        Ok(Some(Err(e))) => anyhow::bail!("ws recv error: {e}"),
        Ok(None) => anyhow::bail!("ws closed before handshake response"),
        Err(_) => anyhow::bail!("handshake response timeout"),
    };

    let handshake_resp: HandshakeResponse =
        serde_json::from_str(resp.as_str()).context("relay handshake JSON")?;
    if !handshake_resp.ok {
        // A server response can echo the authentication token.
        anyhow::bail!("relay rejected handshake");
    }
    let supports_text_keepalive =
        handshake_resp.relay_keepalive.as_deref() == Some("text-v1");
    let keepalive_mode = if supports_text_keepalive { "text-v1" } else { "websocket-ping" };

    info!("Portal relay handshake OK — starting MCP server on WebSocket bridge");
    tool_host.set_connection_state(crate::tools::status::ConnectionState::Connected);
    #[cfg(windows)]
    crate::connection_status::publish("connected");

    let (portal_stream, bridge_stream) = tokio::io::duplex(65536);

    let (ws_write, mut ws_read) = ws.split();
    let ws_write = std::sync::Arc::new(Mutex::new(ws_write));
    let last_seen = std::sync::Arc::new(Mutex::new(Instant::now()));

    let (bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    let mut bridge_reader = tokio::io::BufReader::new(bridge_read);

    let ws_write_inbound = std::sync::Arc::clone(&ws_write);
    let last_seen_inbound = std::sync::Arc::clone(&last_seen);
    let ws_to_bridge = async move {
        while let Some(msg) = ws_read.next().await {
            *last_seen_inbound.lock().await = Instant::now();
            match msg {
                Ok(Message::Text(t)) => {
                    // The liveness timestamp is updated above; never leak the ACK into MCP.
                    if t.starts_with("{\"type\":\"keepalive_ack\"") {
                        continue;
                    }
                    let mut data = t.as_bytes().to_vec();
                    data.push(b'\n');
                    if tokio::io::AsyncWriteExt::write_all(&mut bridge_write, &data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Ping(payload)) => {
                    // tokio-tungstenite may auto-reply when using unsplit stream; we split,
                    // so reply explicitly (D-077).
                    let mut w = ws_write_inbound.lock().await;
                    if w.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    };

    let ws_write_out = std::sync::Arc::clone(&ws_write);
    let bridge_to_ws = async move {
        let mut line = Vec::new();
        loop {
            line.clear();
            match crate::mcp::limits::read_line(&mut bridge_reader, &mut line, 16 * 1024 * 1024).await {
                Ok(0) => break,
                Ok(_) => {
                    let Ok(text) = std::str::from_utf8(&line) else { break; };
                    let trimmed = text.trim();
                    if !trimmed.is_empty()
                        && ws_write_out.lock().await.send(Message::Text(trimmed.to_string())).await.is_err() {
                            break;
                        }
                }
                Err(_) => break,
            }
        }
    };

    let ws_write_hb = std::sync::Arc::clone(&ws_write);
    let last_seen_hb = std::sync::Arc::clone(&last_seen);
    let heartbeat = async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let session_started = Instant::now();
        let mut previous_tick = Instant::now();
        loop {
            interval.tick().await;
            let now = Instant::now();
            let tick_gap = now.saturating_duration_since(previous_tick);
            previous_tick = now;
            if scheduling_gap_detected(tick_gap) {
                warn!(gap_secs = tick_gap.as_secs_f64(), "relay liveness scheduling gap detected");
                anyhow::bail!("relay liveness scheduling gap detected");
            }
            {
                let last_seen = *last_seen_hb.lock().await;
                let inbound_age = now.saturating_duration_since(last_seen);
                if inbound_age
                    > Duration::from_secs(HEARTBEAT_TIMEOUT_SECS)
                {
                    // Closing can itself block behind a congested writer. The
                    // session owner drops the socket when this future returns.
                    warn!(session_age_secs = session_started.elapsed().as_secs(), last_inbound_secs = inbound_age.as_secs(), keepalive_mode, "relay heartbeat response timeout");
                    anyhow::bail!(
                        "no relay heartbeat response within {}s (D-077)",
                        HEARTBEAT_TIMEOUT_SECS
                    );
                }
            }
            let heartbeat_message = if supports_text_keepalive {
                Message::Text("{\"type\":\"keepalive\"}".into())
            } else {
                Message::Ping(b"hp".to_vec())
            };
            // Include lock acquisition: a stuck response writer must not
            // prevent the liveness task from returning control to reconnect.
            tokio::time::timeout(Duration::from_secs(10), async {
                ws_write_hb.lock().await.send(heartbeat_message).await
            }).await.context("relay heartbeat write timed out")??;
        }
    };
    // Keep bridge and handler futures owned by this session. Any disconnect
    // cancels all four together, including pending MCP calls; no detached
    // bridge can retain the socket or deliver an old response after reconnect.
    tokio::select! {
        result = crate::handle_connection(portal_stream, tool_host, portal_name, None) => result,
        result = heartbeat => result,
        _ = ws_to_bridge => anyhow::bail!("relay receive bridge closed"),
        _ = bridge_to_ws => anyhow::bail!("relay send bridge closed"),
    }
}

fn scheduling_gap_detected(gap: Duration) -> bool {
    gap > Duration::from_secs(HEARTBEAT_INTERVAL_SECS * SCHEDULING_GAP_MULTIPLIER as u64)
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

    #[test]
    fn localhost_prefix_does_not_disable_tls_for_remote_hosts() {
        for host in ["localhost.example.com:443", "127.example.com", "127.0.0.1.example.com"] {
            assert_eq!(derive_relay_url("", host), format!("wss://{host}/_relay"));
        }
        assert!(is_loopback_host("127.0.0.1:3100"));
        assert!(is_loopback_host("[::1]:3100"));
    }

    #[test]
    fn relay_handshake_json_shape() {
        let v = relay_handshake_json("being1", "tok", "my-laptop");
        assert_eq!(v["being_id"], "being1");
        assert_eq!(v["loom_token"], "tok");
        assert_eq!(v["portal_name"], "my-laptop");
    }

    #[test]
    fn handshake_response_negotiates_text_keepalive() {
        let response: HandshakeResponse = serde_json::from_str(
            r#"{"ok":true,"being_id":"being1","relay_keepalive":"text-v1"}"#,
        )
        .unwrap();

        assert!(response.ok);
        assert_eq!(response.relay_keepalive.as_deref(), Some("text-v1"));
    }

    #[test]
    fn next_backoff_doubles_and_caps() {
        assert_eq!(next_backoff(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(next_backoff(Duration::from_secs(16)), Duration::from_secs(30));
        assert_eq!(next_backoff(Duration::from_secs(30)), Duration::from_secs(30));
    }

    #[test]
    fn scheduling_gap_requires_more_than_three_heartbeat_intervals() {
        assert!(!scheduling_gap_detected(Duration::from_secs(45)));
        assert!(scheduling_gap_detected(Duration::from_secs(46)));
    }

    #[test]
    fn next_backoff_never_below_floor() {
        assert_eq!(next_backoff(Duration::from_secs(0)), Duration::from_secs(2));
    }

    #[test]
    fn backoff_sleep_applies_jitter_within_bounds() {
        let b = Duration::from_secs(10);
        assert_eq!(backoff_sleep_ms(b, 0.0), 10_000);
        assert_eq!(backoff_sleep_ms(b, 0.3), 13_000);
        assert_eq!(backoff_sleep_ms(b, -0.3), 7_000);
    }

    #[test]
    fn backoff_sleep_clamps_to_one_second() {
        // 2s backoff with max negative jitter is 1.4s; a degenerate 1s backoff
        // with negative jitter must still floor at 1s.
        assert_eq!(backoff_sleep_ms(Duration::from_secs(2), -0.3), 1_400);
        assert_eq!(backoff_sleep_ms(Duration::from_secs(1), -0.3), 1_000);
    }

    #[test]
    fn jitter_ratio_stays_in_range() {
        for _ in 0..100 {
            let r = jitter_ratio();
            assert!((-0.3..0.3).contains(&r), "jitter out of range: {r}");
        }
    }

    #[test]
    fn handshake_response_without_capability_uses_fallback() {
        let response: HandshakeResponse =
            serde_json::from_str(r#"{"ok":true,"being_id":"being1"}"#).unwrap();

        assert!(response.ok);
        assert_eq!(response.relay_keepalive, None);
    }
}
