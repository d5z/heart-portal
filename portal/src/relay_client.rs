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
use tracing::{info, warn, Instrument};
use tokio::time::MissedTickBehavior;

use crate::tools::ToolHost;

/// Portal → relay JSON handshake ping interval (D-077).
/// Reduced from 30s to 15s in v0.7.1 to shrink the no-activity window
/// on platforms (Windows) where network devices may kill idle TCP.
const HEARTBEAT_INTERVAL_SECS: u64 = 15;
/// If no relay frame is received within this window, reconnect (D-077).
const HEARTBEAT_TIMEOUT_SECS: u64 = 90;

#[derive(Deserialize)]
struct HandshakeResponse {
    ok: bool,
    #[serde(default)]
    relay_keepalive: Option<String>,
    #[serde(default)]
    error: serde_json::Value,
    #[serde(default)]
    code: serde_json::Value,
    #[serde(default)]
    message: serde_json::Value,
}

#[derive(Debug)]
struct HandshakeRejected(String);

impl std::fmt::Display for HandshakeRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "relay rejected handshake: {}", self.0)
    }
}
impl std::error::Error for HandshakeRejected {}

impl HandshakeResponse {
    fn rejection(&self, token: &str) -> HandshakeRejected {
        // Select diagnostic fields only; never serialize the response or echoed handshake.
        let fields = [&self.code, &self.message, &self.error,
            &self.error["code"], &self.error["message"]];
        let detail = fields.into_iter().filter_map(|value| value.as_str())
            .collect::<Vec<_>>().join("; ");
        let detail = if token.is_empty() { detail } else { detail.replace(token, "[redacted]") };
        // Redact before truncation, including quoted credential values containing spaces.
        let credentials = regex::Regex::new(r#"(?i)(?:[\w-]*(?:token|secret|password|api[_-]?key)|authorization)["']?\s*[:=]\s*(?:"[^"]*"|'[^']*'|(?:bearer\s+)?[^\s,;&}]+)"#).unwrap();
        let detail = credentials.replace_all(&detail, "[credential redacted]");
        let urls = regex::Regex::new(r#"(?i)(?:https?|wss?)://[^\s"'<>]+"#).unwrap();
        let detail = urls.replace_all(&detail, "[url redacted]");
        let detail: String = detail.chars().map(|c| if c.is_control() { ' ' } else { c }).take(512).collect();
        HandshakeRejected(if detail.trim().is_empty() { "reason_not_provided".into() } else { detail })
    }
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
            crate::connection_status::publish("invalid");
            warn!("invalid Loom link: {e:#}");
            return;
        }
    };
    let relay_url = derive_relay_url(loom_link, &host);
    info!(
        pid = std::process::id(), version = crate::protocol::PORTAL_VERSION,
        "Portal connect mode: relay {} (being_id={}, portal_name={}, host={})",
        relay_url, being_id, portal_name, host
    );

    let mut backoff = Duration::from_secs(BACKOFF_MIN_SECS);
    let mut attempt: u64 = 0;
    loop {
        attempt += 1;
        tool_host.set_connection_state(crate::tools::status::ConnectionState::Connecting);
        crate::connection_status::publish("connecting");
        let session_start = Instant::now();
        let mut rejected = false;
        match run_one_session(&relay_url, &being_id, &token, tool_host, portal_name)
            .instrument(tracing::info_span!("relay_session", attempt, pid = std::process::id())).await {
            Ok(()) => {
                backoff = Duration::from_secs(BACKOFF_MIN_SECS);
                info!("relay session ended cleanly; reconnecting in {:?}", backoff);
            }
            Err(e) => {
                rejected = e.is::<HandshakeRejected>();
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
                        attempt, pid = std::process::id(), elapsed_ms = session_start.elapsed().as_millis() as u64,
                        "relay session error after {}s: {e:#}; retry in {:?}",
                        session_start.elapsed().as_secs(),
                        backoff
                    );
                }
            }
        }
        tool_host.set_connection_state(if rejected { crate::tools::status::ConnectionState::Rejected }
            else { crate::tools::status::ConnectionState::Retrying });
        crate::connection_status::publish(if rejected { "handshake_rejected" } else { "retrying" });
        let retry_ms = backoff_sleep_ms(backoff, jitter_ratio());
        info!(attempt, pid = std::process::id(), retry_ms, handshake_rejected = rejected, "relay retry scheduled");
        tokio::time::sleep(Duration::from_millis(retry_ms)).await;
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
        info!(stage = "tcp_connected", "relay transport ready");
        // TCP keepalive: 15s idle + 5s probe interval (survives NAT/firewall idle timeouts)
        let sock_ref = socket2::SockRef::from(&tcp);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(15))
            .with_interval(Duration::from_secs(5));
        let _ = sock_ref.set_tcp_keepalive(&keepalive);
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
        info!(stage = "websocket_connected", "relay transport ready");
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
        return Err(handshake_resp.rejection(token).into());
    }
    let supports_text_keepalive =
        handshake_resp.relay_keepalive.as_deref() == Some("text-v1");

    info!("Portal relay handshake OK — starting MCP server on WebSocket bridge");
    tool_host.set_connection_state(crate::tools::status::ConnectionState::Connected);
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
                    tokio::io::AsyncWriteExt::write_all(&mut bridge_write, &data).await
                        .context("relay inbound MCP bridge write")?;
                }
                Ok(Message::Ping(payload)) => {
                    // tokio-tungstenite may auto-reply when using unsplit stream; we split,
                    // so reply explicitly (D-077).
                    let mut w = ws_write_inbound.lock().await;
                    w.send(Message::Pong(payload)).await.context("relay pong send")?;
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(frame)) => {
                    // Close reasons are arbitrary server text; retain only the protocol code.
                    anyhow::bail!("relay WebSocket closed (code={})", frame.map(|f| u16::from(f.code)).unwrap_or(1005));
                }
                Ok(_) => {}
                Err(error) => return Err(anyhow::Error::new(error).context("relay WebSocket receive")),
            }
        }
        anyhow::bail!("relay WebSocket reached EOF")
    };

    let ws_write_out = std::sync::Arc::clone(&ws_write);
    let bridge_to_ws = async move {
        let mut line = Vec::new();
        loop {
            line.clear();
            match crate::mcp::limits::read_line(&mut bridge_reader, &mut line, 16 * 1024 * 1024).await {
                Ok(0) => anyhow::bail!("relay outbound MCP bridge reached EOF"),
                Ok(_) => {
                    let text = std::str::from_utf8(&line).context("relay outbound MCP bridge UTF-8")?;
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        ws_write_out.lock().await.send(Message::Text(trimmed.to_string())).await
                            .context("relay WebSocket send")?;
                    }
                }
                Err(error) => return Err(error.context("relay outbound MCP bridge read")),
            }
        }
    };

    let ws_write_hb = std::sync::Arc::clone(&ws_write);
    let last_seen_hb = std::sync::Arc::clone(&last_seen);
    let heartbeat = async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            {
                let last_seen = *last_seen_hb.lock().await;
                if Instant::now().saturating_duration_since(last_seen)
                    > Duration::from_secs(HEARTBEAT_TIMEOUT_SECS)
                {
                    // Closing can itself block behind a congested writer. The
                    // session owner drops the socket when this future returns.
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
        result = ws_to_bridge => result,
        result = bridge_to_ws => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_reason_is_bounded_and_does_not_echo_credentials() {
        let response: HandshakeResponse = serde_json::from_value(serde_json::json!({
            "ok": false, "code": "invalid_token",
            "error": { "message": "Denied supplied-value\nAuthorization: Bearer another-secret; password='has spaces'; https://host/?token=url-secret" },
            "loom_token": "echoed-secret", "request": { "token": "nested-secret" }
        })).unwrap();
        let reason = response.rejection("supplied-value").to_string();
        assert!(reason.contains("invalid_token"));
        for secret in ["supplied-value", "another-secret", "has spaces", "url-secret", "echoed-secret", "nested-secret", "\n"] {
            assert!(!reason.contains(secret), "leaked {secret}: {reason}");
        }
        let missing: HandshakeResponse = serde_json::from_str(r#"{"ok":false}"#).unwrap();
        assert!(missing.rejection("secret").to_string().contains("reason_not_provided"));
        let long: HandshakeResponse = serde_json::from_value(serde_json::json!({
            "ok": false, "error": "拒绝".repeat(1000)
        })).unwrap();
        assert_eq!(long.rejection("secret").0.chars().count(), 512);
    }

    #[tokio::test]
    async fn websocket_rejection_is_typed_and_a_later_session_can_recover() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/_relay", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for ok in [false, true] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
                let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
                assert_eq!(hello["loom_token"], "fixture-secret");
                ws.send(Message::Text(serde_json::json!({
                    "ok": ok, "error": "invalid credential fixture-secret"
                }).to_string())).await.unwrap();
                if ok { tokio::time::sleep(Duration::from_millis(50)).await; }
                ws.close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy,
                    reason: "fixture-secret".into(),
                })).await.unwrap();
            }
        });
        let root = crate::kits::tests::TestKits::new();
        let mut config = crate::config::PortalConfig::default();
        config.security.workspace_root = root.0.clone();
        config.kits_enabled = false;
        config.subagent.enabled = false;
        let host = ToolHost::new(&config);
        let error = run_one_session(&url, "fixture", "fixture-secret", &host, "test").await.unwrap_err();
        assert!(error.is::<HandshakeRejected>());
        assert!(error.to_string().contains("invalid credential"));
        assert!(!error.to_string().contains("fixture-secret"));
        let closed = run_one_session(&url, "fixture", "fixture-secret", &host, "test").await.unwrap_err();
        assert!(!closed.is::<HandshakeRejected>());
        assert!(closed.to_string().contains("code=1008"), "{closed:#}");
        assert!(!closed.to_string().contains("fixture-secret"));
        server.await.unwrap();
    }

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
