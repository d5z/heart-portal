//! JSONL client for the pi daemon (PRD §4.5).
//!
//! One socket multiplexes every command for every session. A single reader task
//! owns the read half: it correlates `response` lines to waiting requests by
//! `id` and broadcasts everything else as an event. The reader never awaits a
//! handler, so one slow consumer cannot stall the protocol (PRD §9 risk 7).
//!
//! Transports: Unix socket on unix (the daemon's native transport), piped stdio
//! elsewhere. Both funnel into [`PiClient::connect_io`].

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex, Notify};
use tokio::time;
use tracing::{debug, trace, warn};

use super::protocol::{
    Command, CommandResponse, DaemonHello, DaemonMessage, MAX_LINE_BYTES, PINNED_PI_VERSION,
};

/// How long to wait for `daemon_hello` after the transport is up.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// Probe connect budget when checking whether a daemon is already alive.
pub const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
/// Default request timeout for metadata commands.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Event fan-out depth. Lagging consumers drop old events rather than block.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// How strict [`PiClient::connect_io`] is about the greeting.
#[derive(Debug, Clone, Copy)]
pub struct ConnectPolicy {
    /// Fail if no `daemon_hello` arrives within [`HELLO_TIMEOUT`].
    pub require_hello: bool,
    /// Fail on a protocol name/version Portal does not implement.
    pub strict_protocol: bool,
}

impl ConnectPolicy {
    /// Normal use: a usable v7 daemon or nothing.
    pub const STRICT: Self = Self {
        require_hello: true,
        strict_protocol: true,
    };
    /// Adoption probe: get the greeting, let the caller judge the version so a
    /// wrong-version daemon can be shut down instead of merely rejected.
    pub const PROBE: Self = Self {
        require_hello: true,
        strict_protocol: false,
    };
    /// Degraded stdio transport that may never greet.
    #[allow(dead_code)] // only reachable on the non-unix stdio fallback
    pub const LENIENT: Self = Self {
        require_hello: false,
        strict_protocol: false,
    };
}

/// A connected pi daemon client.
pub struct PiClient {
    hello: DaemonHello,
    client_id: String,
    label: String,
    writer: AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Mutex<HashMap<String, oneshot::Sender<CommandResponse>>>,
    events: broadcast::Sender<DaemonMessage>,
    connected: Arc<AtomicBool>,
    disconnected: Arc<Notify>,
    next_id: AtomicU64,
}

impl PiClient {
    /// Connect over the daemon's Unix socket and complete the greeting.
    #[cfg(unix)]
    pub async fn connect_unix(path: &Path) -> Result<Arc<Self>> {
        let stream = time::timeout(
            Duration::from_secs(5),
            tokio::net::UnixStream::connect(path),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to {}", path.display()))?
        .with_context(|| format!("connecting to pi daemon socket {}", path.display()))?;
        let (read_half, write_half) = stream.into_split();
        Self::connect_io(
            read_half,
            write_half,
            path.display().to_string(),
            ConnectPolicy::STRICT,
        )
        .await
    }

    /// Cheap liveness probe: connect, read the greeting, hand back the client.
    /// `Err` means "no usable daemon here" — the caller spawns one.
    #[cfg(unix)]
    pub async fn probe_unix(path: &Path) -> Result<Arc<Self>> {
        if !path.exists() {
            anyhow::bail!("no socket at {}", path.display());
        }
        let stream = time::timeout(
            PROBE_CONNECT_TIMEOUT,
            tokio::net::UnixStream::connect(path),
        )
        .await
        .map_err(|_| anyhow::anyhow!("probe timed out connecting to {}", path.display()))?
        .with_context(|| format!("probing pi daemon socket {}", path.display()))?;
        let (read_half, write_half) = stream.into_split();
        Self::connect_io(
            read_half,
            write_half,
            path.display().to_string(),
            ConnectPolicy::PROBE,
        )
        .await
    }

    /// Transport-agnostic constructor.
    pub async fn connect_io<R, W>(
        reader: R,
        writer: W,
        label: String,
        policy: ConnectPolicy,
    ) -> Result<Arc<Self>>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let mut reader = BufReader::new(reader);

        let hello = match time::timeout(HELLO_TIMEOUT, read_hello(&mut reader)).await {
            Ok(Ok(hello)) => {
                if policy.strict_protocol {
                    hello.check_protocol()?;
                }
                if let Some(version) = hello.app_version.as_deref() {
                    if version != PINNED_PI_VERSION {
                        warn!(
                            "pi {} differs from the pinned version {}; protocol drift is possible",
                            version, PINNED_PI_VERSION
                        );
                    }
                }
                hello
            }
            Ok(Err(e)) if policy.require_hello => return Err(e),
            Err(_) if policy.require_hello => {
                anyhow::bail!("pi daemon at {label} sent no daemon_hello within {HELLO_TIMEOUT:?}")
            }
            _ => {
                warn!("pi at {label} sent no greeting; continuing unverified");
                DaemonHello::unverified()
            }
        };

        let client_id = hello
            .client_id
            .clone()
            .unwrap_or_else(|| format!("portal-{}", uuid::Uuid::new_v4()));
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let connected = Arc::new(AtomicBool::new(true));
        let disconnected = Arc::new(Notify::new());

        let client = Arc::new(Self {
            hello,
            client_id,
            label: label.clone(),
            writer: AsyncMutex::new(Box::new(writer)),
            pending: Mutex::new(HashMap::new()),
            events: events.clone(),
            connected: Arc::clone(&connected),
            disconnected: Arc::clone(&disconnected),
            next_id: AtomicU64::new(1),
        });

        // The reader task must not hold an Arc to the client, or the client can
        // never be dropped; it owns only the pieces it touches.
        let pending_handle = Arc::downgrade(&client);
        tokio::spawn(async move {
            reader_task(reader, pending_handle, events, &label).await;
            connected.store(false, Ordering::SeqCst);
            disconnected.notify_waiters();
        });

        Ok(client)
    }

    pub fn hello(&self) -> &DaemonHello {
        &self.hello
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// True when connected to a daemon (vs stdio pipe).
    /// Daemon transport requires protocol-7 command envelopes.
    fn uses_envelope(&self) -> bool {
        self.hello.protocol.name == super::protocol::DAEMON_PROTOCOL_NAME
    }

    /// Serialize a command for this client's transport: envelope for daemon, bare for stdio.
    fn serialize_command(&self, cmd: &Command, id: &str) -> anyhow::Result<String> {
        if self.uses_envelope() {
            cmd.to_envelope(id, &self.client_id)
        } else {
            cmd.to_line(id)
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Resolves when the reader task ends (EOF, `daemon_closing`, or error).
    #[allow(dead_code)] // `is_connected` covers current callers; kept for recovery work
    pub async fn wait_disconnected(&self) {
        if !self.is_connected() {
            return;
        }
        self.disconnected.notified().await;
    }

    /// Every non-response line the daemon writes. Lagging receivers skip.
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonMessage> {
        self.events.subscribe()
    }

    /// Send a command and await its correlated response.
    /// `Err` on transport failure, timeout, or a `success:false` reply.
    pub async fn request(&self, cmd: Command, timeout: Duration) -> Result<Value> {
        if !self.is_connected() {
            anyhow::bail!("pi daemon connection to {} is closed", self.label);
        }

        let name = cmd.name();
        let id = format!("portal-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let line = self.serialize_command(&cmd, &id)?;
        let (tx, rx) = oneshot::channel();
        self.insert_pending(id.clone(), tx);

        if let Err(e) = self.write_line(&line).await {
            self.take_pending(&id);
            return Err(e).with_context(|| format!("sending pi {name}"));
        }
        trace!("pi → {name} ({id})");

        match time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => resp.into_data(),
            Ok(Err(_)) => {
                anyhow::bail!("pi daemon closed the connection while running {name}")
            }
            Err(_) => {
                self.take_pending(&id);
                anyhow::bail!("pi {name} did not answer within {timeout:?}")
            }
        }
    }

    /// Fire-and-forget: send the command, do not wait for the reply.
    /// Used where a failure changes nothing (declining a dialog, detaching).
    pub async fn notify(&self, cmd: Command) {
        let name = cmd.name();
        let id = format!("portal-n{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        match self.serialize_command(&cmd, &id) {
            Ok(line) => {
                if let Err(e) = self.write_line(&line).await {
                    debug!("pi {name} (fire-and-forget) failed: {e:#}");
                }
            }
            Err(e) => warn!("could not serialize pi {name}: {e:#}"),
        }
    }

    async fn write_line(&self, line: &str) -> Result<()> {
        if line.len() > MAX_LINE_BYTES {
            anyhow::bail!("outbound protocol line exceeds {MAX_LINE_BYTES} bytes");
        }
        let mut writer = self.writer.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok(())
    }

    fn insert_pending(&self, id: String, tx: oneshot::Sender<CommandResponse>) {
        match self.pending.lock() {
            Ok(mut g) => {
                g.insert(id, tx);
            }
            Err(e) => warn!("pi pending map poisoned: {e}"),
        }
    }

    fn take_pending(&self, id: &str) -> Option<oneshot::Sender<CommandResponse>> {
        self.pending.lock().ok().and_then(|mut g| g.remove(id))
    }

    /// Fail every in-flight request; called when the reader task ends so
    /// callers get an error instead of hanging until their timeout.
    fn drain_pending(&self) {
        if let Ok(mut g) = self.pending.lock() {
            g.clear();
        }
    }
}

/// Read and parse the greeting, skipping any line that is not a `daemon_hello`.
async fn read_hello<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<DaemonHello> {
    let mut buf = Vec::new();
    loop {
        if read_line_capped(reader, &mut buf).await? == 0 {
            anyhow::bail!("pi daemon closed the connection before greeting");
        }
        let line = String::from_utf8_lossy(&buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<DaemonMessage>(trimmed) {
            Ok(DaemonMessage::Hello(hello)) => return Ok(hello),
            Ok(other) => debug!("ignoring {other:?} received before daemon_hello"),
            Err(e) => anyhow::bail!("pi daemon sent invalid JSON as its first line: {e}"),
        }
    }
}

async fn reader_task<R: AsyncBufRead + Unpin>(
    mut reader: R,
    client: std::sync::Weak<PiClient>,
    events: broadcast::Sender<DaemonMessage>,
    label: &str,
) {
    let mut buf = Vec::new();
    loop {
        match read_line_capped(&mut reader, &mut buf).await {
            Ok(0) => {
                debug!("pi daemon at {label} closed the connection (EOF)");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("pi daemon at {label} read error: {e}");
                break;
            }
        }

        let line = String::from_utf8_lossy(&buf);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: DaemonMessage = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                warn!("pi daemon at {label} sent invalid JSON ({e}); ignoring the line");
                continue;
            }
        };

        match msg {
            DaemonMessage::Response(resp) => {
                let Some(client) = client.upgrade() else {
                    debug!("pi client dropped; stopping reader for {label}");
                    return;
                };
                trace!("pi ← response {} ({})", resp.command, resp.id);
                match client.take_pending(&resp.id) {
                    Some(tx) => {
                        let _ = tx.send(resp);
                    }
                    None => debug!(
                        "pi response {} for unknown request id {}",
                        resp.command, resp.id
                    ),
                }
            }
            DaemonMessage::Unknown => trace!("pi ← (unmodelled message, ignored)"),
            other => {
                // No consumer yet, or a lagging one, is not an error.
                let _ = events.send(other);
            }
        }
    }

    if let Some(client) = client.upgrade() {
        client.drain_pending();
    }
}

/// Read one `\n`-terminated line into `buf` (newline excluded).
/// Returns 0 only at EOF. Errors past [`MAX_LINE_BYTES`] rather than growing
/// without bound on a malformed stream.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<usize> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // EOF: a trailing line without a newline still counts.
            return Ok(buf.len());
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                buf.extend_from_slice(&available[..idx]);
                reader.consume(idx + 1);
                // +1 so an empty line is distinguishable from EOF.
                return Ok(buf.len() + 1);
            }
            None => {
                let n = available.len();
                buf.extend_from_slice(available);
                reader.consume(n);
                if buf.len() > MAX_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("pi protocol line exceeds {MAX_LINE_BYTES} bytes"),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::protocol::LIFECYCLE_RESIDENT;
    use tokio::io::DuplexStream;

    const HELLO_LINE: &str = r#"{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7},"appVersion":"0.7.2","supervisorPid":4242,"serverCapabilities":["session_input_admission"]}"#;

    /// A fake daemon on an in-memory duplex: greets, then lets the test script
    /// replies per received command line.
    struct FakeDaemon {
        reader: BufReader<DuplexStream>,
        writer: DuplexStream,
    }

    impl FakeDaemon {
        async fn write_line(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn next_command(&mut self) -> Value {
            let mut buf = Vec::new();
            let n = read_line_capped(&mut self.reader, &mut buf).await.unwrap();
            assert_ne!(n, 0, "client closed the connection");
            let raw: Value = serde_json::from_slice(&buf).unwrap();
            // Unwrap protocol-7 envelope if present.
            if raw.get("type").and_then(|t| t.as_str()) == Some("command") {
                let mut inner = raw["command"].clone();
                // Hoist the envelope id into the inner command so callers see it.
                if let Some(id) = raw.get("id") {
                    inner["id"] = id.clone();
                }
                inner
            } else {
                raw
            }
        }
    }

    /// Returns (client, fake daemon). `hello` is written before connecting so
    /// the greeting is already buffered.
    async fn connect_pair(hello: Option<&str>) -> (Arc<PiClient>, FakeDaemon) {
        // One duplex pair per direction: daemon_tx → client_rx, client_tx → daemon_rx.
        let (client_rx, mut daemon_tx) = tokio::io::duplex(64 * 1024);
        let (daemon_rx, client_tx) = tokio::io::duplex(64 * 1024);

        if let Some(hello) = hello {
            daemon_tx.write_all(hello.as_bytes()).await.unwrap();
            daemon_tx.write_all(b"\n").await.unwrap();
            daemon_tx.flush().await.unwrap();
        }

        let client = PiClient::connect_io(client_rx, client_tx, "test".to_string(), ConnectPolicy::STRICT)
            .await
            .expect("connect");
        (
            client,
            FakeDaemon {
                reader: BufReader::new(daemon_rx),
                writer: daemon_tx,
            },
        )
    }

    #[tokio::test]
    async fn greeting_is_parsed_and_capabilities_exposed() {
        let (client, _daemon) = connect_pair(Some(HELLO_LINE)).await;
        assert!(client.is_connected());
        assert_eq!(client.hello().app_version.as_deref(), Some("0.7.2"));
        assert_eq!(client.hello().supervisor_pid, Some(4242));
        assert!(client.hello().supports("session_input_admission"));
    }

    #[tokio::test]
    async fn wrong_protocol_version_refuses_to_connect() {
        let (client_rx, mut daemon_tx) = tokio::io::duplex(4096);
        let (_daemon_rx, client_tx) = tokio::io::duplex(4096);
        daemon_tx
            .write_all(
                b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":6}}\n",
            )
            .await
            .unwrap();
        let err = match PiClient::connect_io(client_rx, client_tx, "test".into(), ConnectPolicy::STRICT).await {
            Ok(_) => panic!("a v6 daemon must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("protocol v6"), "{err}");
    }

    #[tokio::test]
    async fn requests_are_correlated_by_id_even_out_of_order() {
        let (client, mut daemon) = connect_pair(Some(HELLO_LINE)).await;

        let c1 = Arc::clone(&client);
        let first = tokio::spawn(async move {
            c1.request(
                Command::GetState {
                    active_session_id: "as_1".into(),
                },
                Duration::from_secs(5),
            )
            .await
        });
        let c2 = Arc::clone(&client);
        let second = tokio::spawn(async move {
            c2.request(
                Command::GetLastAssistantText {
                    active_session_id: "as_1".into(),
                },
                Duration::from_secs(5),
            )
            .await
        });

        let a = daemon.next_command().await;
        let b = daemon.next_command().await;
        let (state_id, text_id) = if a["type"] == "get_state" {
            (a["id"].as_str().unwrap().to_string(), b["id"].as_str().unwrap().to_string())
        } else {
            (b["id"].as_str().unwrap().to_string(), a["id"].as_str().unwrap().to_string())
        };

        // Answer in reverse order on purpose.
        daemon
            .write_line(&format!(
                r###"{{"id":"{text_id}","type":"response","command":"get_last_assistant_text","success":true,"data":"## Result"}}"###
            ))
            .await;
        daemon
            .write_line(&format!(
                r#"{{"id":"{state_id}","type":"response","command":"get_state","success":true,"data":{{"isStreaming":true}}}}"#
            ))
            .await;

        assert_eq!(
            second.await.unwrap().unwrap(),
            Value::String("## Result".to_string())
        );
        assert_eq!(first.await.unwrap().unwrap()["isStreaming"], true);
    }

    #[tokio::test]
    async fn failed_response_surfaces_the_daemon_error() {
        let (client, mut daemon) = connect_pair(Some(HELLO_LINE)).await;
        let c = Arc::clone(&client);
        let call = tokio::spawn(async move {
            c.request(
                Command::Create {
                    name: "portal:x".into(),
                    lifecycle: LIFECYCLE_RESIDENT.into(),
                    session_path: None,
                    config: Default::default(),
                },
                Duration::from_secs(5),
            )
            .await
        });
        let cmd = daemon.next_command().await;
        assert_eq!(cmd["type"], "create");
        daemon
            .write_line(&format!(
                r#"{{"id":"{}","type":"response","command":"create","success":false,"error":"no provider configured"}}"#,
                cmd["id"].as_str().unwrap()
            ))
            .await;
        let err = call.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("no provider configured"), "{err}");
    }

    #[tokio::test]
    async fn events_are_broadcast_and_unknown_messages_dropped() {
        let (client, mut daemon) = connect_pair(Some(HELLO_LINE)).await;
        let mut rx = client.subscribe();

        daemon.write_line(r#"{"type":"snapshot_chunk","seq":1}"#).await;
        daemon
            .write_line(
                r#"{"type":"session_event","activeSessionId":"as_1","event":{"type":"agent_start"}}"#,
            )
            .await;

        let msg = time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event")
            .expect("recv");
        match msg {
            DaemonMessage::SessionEvent(ev) => assert_eq!(ev.event.kind, "agent_start"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_line_does_not_kill_the_reader() {
        let (client, mut daemon) = connect_pair(Some(HELLO_LINE)).await;
        let mut rx = client.subscribe();
        daemon.write_line("not json at all").await;
        daemon
            .write_line(r#"{"type":"session_closed","activeSessionId":"as_1","reason":"completed"}"#)
            .await;

        let msg = time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event")
            .expect("recv");
        assert!(matches!(msg, DaemonMessage::SessionClosed(_)));
        assert!(client.is_connected());
    }

    #[tokio::test]
    async fn request_times_out_without_leaking_the_pending_entry() {
        let (client, mut daemon) = connect_pair(Some(HELLO_LINE)).await;
        let err = client
            .request(
                Command::GetState {
                    active_session_id: "as_1".into(),
                },
                Duration::from_millis(80),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not answer"), "{err}");
        let _ = daemon.next_command().await;
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disconnect_unblocks_in_flight_requests() {
        let (client, daemon) = connect_pair(Some(HELLO_LINE)).await;
        let c = Arc::clone(&client);
        let call = tokio::spawn(async move {
            c.request(
                Command::WaitForIdle {
                    active_session_id: "as_1".into(),
                },
                Duration::from_secs(30),
            )
            .await
        });
        time::sleep(Duration::from_millis(50)).await;
        drop(daemon); // daemon dies mid-task

        let err = time::timeout(Duration::from_secs(5), call)
            .await
            .expect("must not hang until the request timeout")
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed the connection"), "{err}");
        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn oversized_line_is_rejected() {
        let mut oversized = Vec::with_capacity(MAX_LINE_BYTES + 16);
        oversized.resize(MAX_LINE_BYTES + 8, b'x');
        let mut reader = BufReader::new(io::Cursor::new(oversized));
        let mut buf = Vec::new();
        let err = read_line_capped(&mut reader, &mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn read_line_capped_splits_lines_and_reports_eof() {
        let mut reader = BufReader::new(io::Cursor::new(b"a\n\nbc".to_vec()));
        let mut buf = Vec::new();
        assert_eq!(read_line_capped(&mut reader, &mut buf).await.unwrap(), 2);
        assert_eq!(buf, b"a");
        assert_eq!(read_line_capped(&mut reader, &mut buf).await.unwrap(), 1);
        assert!(buf.is_empty(), "empty line is not EOF");
        assert_eq!(read_line_capped(&mut reader, &mut buf).await.unwrap(), 2);
        assert_eq!(buf, b"bc");
        assert_eq!(read_line_capped(&mut reader, &mut buf).await.unwrap(), 0);
    }
}
