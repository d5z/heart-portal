//! Cowork — HTTP/WS server for being's home UI.
//!
//! Serves the Cowork single-page app, file CRUD, WebSocket file-watch,
//! and proxies chat/status/history to Core.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Query, RawQuery, State, ws};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::Multipart;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use heart_shared::{ct_eq_str, optional_portal_cowork_token};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{info, warn, error};

use crate::config::PortalConfig;

/// Max JSON body for chat proxy and similar POSTs (1 MiB).
const MAX_COWORK_JSON_BODY: usize = 1024 * 1024;
/// First WebSocket text frame for token auth (defends against huge frames).
const WS_AUTH_FIRST_MSG_MAX: usize = 8192;

static COWORK_HTML: &str = include_str!("../cowork.html");

// --- Shared state ---

#[derive(Clone)]
pub struct CoworkState {
    pub config: PortalConfig,
    pub workspace: PathBuf,
    pub file_events: broadcast::Sender<FileEvent>,
    pub http_client: reqwest::Client,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub path: String,
}

// --- Query params ---

#[derive(Deserialize)]
pub struct PathQuery {
    pub path: Option<String>,
}

// --- File tree entry ---

#[derive(Serialize)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<FileEntry>>,
}

// --- Auth middleware ---
//
// When PORTAL_TOKEN / LOOM_TOKEN is unset, all Cowork HTTP routes (except unauthenticated
// static HTML) accept requests without a Bearer token — same-origin browsers can call the API.
// That is intentional for local/dev use; for any network-exposed deployment, set a token.

/// Bearer token for Cowork HTTP and WebSocket (PORTAL_TOKEN, else LOOM_TOKEN).
pub(crate) fn cowork_token() -> Option<String> {
    optional_portal_cowork_token()
}

fn check_auth(headers: &HeaderMap) -> Result<(), StatusCode> {
    let Some(token) = cowork_token() else {
        return Ok(());
    };
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(bearer) = auth.strip_prefix("Bearer ") else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if ct_eq_str(bearer.trim(), &token) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn ws_token_matches(query_token: Option<&str>, expected: &str) -> bool {
    query_token.is_some_and(|t| ct_eq_str(t, expected))
}

fn parse_ws_first_message_auth(text: &str, expected: &str) -> bool {
    if ct_eq_str(text.trim(), expected) {
        return true;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(t) = v.get("token").and_then(|x| x.as_str()) {
            return ct_eq_str(t, expected);
        }
        if let Some(t) = v.get("auth").and_then(|x| x.as_str()) {
            return ct_eq_str(t, expected);
        }
    }
    false
}

fn apply_core_auth(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(t) if !t.is_empty() => req.header(header::AUTHORIZATION, format!("Bearer {}", t)),
        _ => req,
    }
}

// --- Path safety ---

fn safe_path(workspace: &Path, rel: &str) -> Result<PathBuf, StatusCode> {
    if rel.is_empty() || rel == "." {
        return Ok(workspace.to_path_buf());
    }
    let joined = workspace.join(rel);
    // For non-existent paths, check parent
    let check = if joined.exists() {
        joined.canonicalize().map_err(|_| StatusCode::BAD_REQUEST)?
    } else {
        let parent = joined.parent().ok_or(StatusCode::BAD_REQUEST)?;
        if !parent.exists() {
            return Err(StatusCode::NOT_FOUND);
        }
        let canon_parent = parent.canonicalize().map_err(|_| StatusCode::BAD_REQUEST)?;
        canon_parent.join(joined.file_name().ok_or(StatusCode::BAD_REQUEST)?)
    };
    let ws_canon = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    if check.starts_with(&ws_canon) {
        Ok(check)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

// --- Router ---

pub fn cowork_router(state: CoworkState) -> Router {
    Router::new()
        .route("/", get(serve_html))
        .route("/api/health", get(api_health))
        .route("/api/soul", get(api_soul))
        .route("/api/files", get(api_files))
        .route("/api/file", get(api_file_read).put(api_file_write).delete(api_file_delete))
        .route("/api/upload", post(api_upload))
        .route("/api/mkdir", post(api_mkdir))
        .route("/api/chat", post(api_chat_proxy))
        .route("/api/status", get(api_proxy_get))
        .route("/api/history", get(api_history_proxy))
        .route("/ws", get(ws_handler))
        .layer(RequestBodyLimitLayer::new(MAX_COWORK_JSON_BODY))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// --- Handlers ---

async fn serve_html() -> Html<&'static str> {
    Html(COWORK_HTML)
}

async fn api_health(State(st): State<CoworkState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "name": st.config.name,
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn api_soul(
    State(st): State<CoworkState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // When a token is configured, require auth so workspace path is not world-readable.
    if cowork_token().is_some() {
        check_auth(&headers)?;
    }
    Ok(Json(serde_json::json!({
        "name": st.config.name,
        "version": env!("CARGO_PKG_VERSION"),
        "workspace": st.workspace.display().to_string()
    })))
}

async fn api_files(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<Json<Vec<FileEntry>>, StatusCode> {
    check_auth(&headers)?;
    let base = safe_path(&st.workspace, q.path.as_deref().unwrap_or(""))?;
    let entries = list_dir_recursive(base, st.workspace.clone(), 3)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(entries))
}

// Synchronous tree walk; run via `list_dir_recursive` + `spawn_blocking` to avoid blocking Tokio.
fn list_dir_recursive_sync(dir: &Path, workspace: &Path, depth: u32) -> std::io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return Ok(entries);
    }
    let mut read = std::fs::read_dir(dir)?;
    while let Some(Ok(e)) = read.next() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // skip hidden
        }
        let meta = e.metadata()?;
        let rel = e.path().strip_prefix(workspace)
            .unwrap_or(e.path().as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let mtime = meta.modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let children = if meta.is_dir() && depth > 0 {
            Some(list_dir_recursive_sync(&e.path(), workspace, depth - 1)?)
        } else {
            None
        };
        entries.push(FileEntry {
            path: rel,
            name,
            is_dir: meta.is_dir(),
            size: meta.len(),
            mtime,
            children,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

async fn list_dir_recursive(
    dir: PathBuf,
    workspace: PathBuf,
    depth: u32,
) -> std::io::Result<Vec<FileEntry>> {
    tokio::task::spawn_blocking(move || list_dir_recursive_sync(&dir, &workspace, depth))
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        .flatten()
}

async fn api_file_read(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<Response, StatusCode> {
    check_auth(&headers)?;
    let path = safe_path(&st.workspace, q.path.as_deref().ok_or(StatusCode::BAD_REQUEST)?)?;
    if !path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    // For images, serve with proper content type
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => {
            let ct = match ext {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "svg" => "image/svg+xml",
                "webp" => "image/webp",
                _ => "application/octet-stream",
            };
            let data = tokio::fs::read(&path).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Response::builder()
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(data))
                .unwrap())
        }
        _ => {
            let content = tokio::fs::read_to_string(&path).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Response::builder()
                .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(Body::from(content))
                .unwrap())
        }
    }
}

async fn api_file_write(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
    body: String,
) -> Result<StatusCode, StatusCode> {
    check_auth(&headers)?;
    let rel = q.path.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    if body.len() > st.config.security.max_file_size {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let path = safe_path(&st.workspace, rel)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    tokio::fs::write(&path, &body).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn api_file_delete(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<StatusCode, StatusCode> {
    check_auth(&headers)?;
    let rel = q.path.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    let path = safe_path(&st.workspace, rel)?;
    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Move to .trash/
    let trash = st.workspace.join(".trash");
    tokio::fs::create_dir_all(&trash).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let dest = trash.join(path.file_name().ok_or(StatusCode::BAD_REQUEST)?);
    tokio::fs::rename(&path, &dest).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn api_upload(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
    mut multipart: Multipart,
) -> Result<StatusCode, StatusCode> {
    check_auth(&headers)?;
    let base_rel = q.path.as_deref().unwrap_or("");

    while let Some(field) = multipart.next_field().await.map_err(|_| StatusCode::BAD_REQUEST)? {
        let filename = field.file_name().unwrap_or("upload").to_string();
        let rel = if base_rel.is_empty() { filename.clone() } else { format!("{}/{}", base_rel, filename) };
        let path = safe_path(&st.workspace, &rel)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        }
        let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;
        if data.len() > st.config.security.max_file_size {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        tokio::fs::write(&path, &data).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    Ok(StatusCode::OK)
}

async fn api_mkdir(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<StatusCode, StatusCode> {
    check_auth(&headers)?;
    let rel = q.path.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
    let path = safe_path(&st.workspace, rel)?;
    tokio::fs::create_dir_all(&path).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

// --- Core proxy ---

async fn api_chat_proxy(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    check_auth(&headers)?;
    let url = format!("{}/api/chat/stream", st.config.cowork.core_url);
    let token = cowork_token();
    let resp = apply_core_auth(
        st.http_client.post(&url).header("Content-Type", "application/json"),
        token.as_deref(),
    )
        .body(body)
        .send()
        .await
        .map_err(|e| {
            error!("Core proxy error: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

    // Stream the response through without buffering
    let status = resp.status();
    let mut builder = Response::builder().status(status.as_u16());
    // Forward content-type
    if let Some(ct) = resp.headers().get("content-type") {
        builder = builder.header("content-type", ct);
    }
    builder = builder.header("cache-control", "no-cache");
    let stream = resp.bytes_stream();
    Ok(builder.body(Body::from_stream(stream)).unwrap())
}

async fn api_proxy_get(
    State(st): State<CoworkState>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_auth(&headers)?;
    let url = format!("{}/api/status", st.config.cowork.core_url);
    proxy_get(&st.http_client, &url, cowork_token().as_deref()).await
}

async fn api_history_proxy(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
) -> Result<Response, StatusCode> {
    check_auth(&headers)?;
    let mut url = format!("{}/api/history", st.config.cowork.core_url);
    if let Some(qs) = q {
        url.push('?');
        url.push_str(&qs);
    }
    proxy_get(&st.http_client, &url, cowork_token().as_deref()).await
}

async fn proxy_get(client: &reqwest::Client, url: &str, token: Option<&str>) -> Result<Response, StatusCode> {
    let resp = apply_core_auth(client.get(url), token)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let status = resp.status();
    let body = resp.text().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(Response::builder()
        .status(status.as_u16())
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap())
}

// --- WebSocket ---

#[derive(Deserialize)]
struct WsAuthQuery {
    token: Option<String>,
}

async fn ws_handler(
    State(st): State<CoworkState>,
    Query(q): Query<WsAuthQuery>,
    upgrade: ws::WebSocketUpgrade,
) -> impl IntoResponse {
    match cowork_token() {
        None => upgrade.on_upgrade(move |socket| handle_ws(socket, st)),
        Some(expected) => {
            if ws_token_matches(q.token.as_deref(), &expected) {
                return upgrade.on_upgrade(move |socket| handle_ws(socket, st));
            }
            // Wrong query token: reject before upgrade
            if q.token.is_some() {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            upgrade.on_upgrade(move |socket| handle_ws_first_message_auth(socket, st, expected))
        }
    }
}

async fn handle_ws_first_message_auth(
    socket: ws::WebSocket,
    st: CoworkState,
    expected: String,
) {
    let (mut sender, mut receiver) = socket.split();
    let first = receiver.next().await;
    let ok = match first {
        Some(Ok(ws::Message::Text(t))) if t.len() <= WS_AUTH_FIRST_MSG_MAX => {
            parse_ws_first_message_auth(&t, &expected)
        }
        _ => false,
    };
    if !ok {
        let frame = ws::CloseFrame {
            code: 4001,
            reason: ws::Utf8Bytes::from_static("unauthorized"),
        };
        let _ = sender.send(ws::Message::Close(Some(frame))).await;
        return;
    }

    run_ws_file_watch(sender, receiver, st).await;
}

async fn handle_ws(socket: ws::WebSocket, st: CoworkState) {
    let (sender, receiver) = socket.split();
    run_ws_file_watch(sender, receiver, st).await;
}

async fn run_ws_file_watch(
    mut sender: SplitSink<ws::WebSocket, ws::Message>,
    mut receiver: SplitStream<ws::WebSocket>,
    st: CoworkState,
) {
    let mut rx = st.file_events.subscribe();

    loop {
        tokio::select! {
            biased;
            recv_msg = receiver.next() => {
                match recv_msg {
                    None => break,
                    Some(Ok(ws::Message::Close(_))) => break,
                    Some(Ok(ws::Message::Text(t))) if t.len() > 65536 => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            recv_ev = rx.recv() => {
                let event = match recv_ev {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let msg = serde_json::to_string(&event).unwrap_or_default();
                if sender.send(ws::Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

// --- File watcher ---

pub fn start_file_watcher(workspace: PathBuf, tx: broadcast::Sender<FileEvent>) {
    std::thread::spawn(move || {
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        let mut watcher = match RecommendedWatcher::new(notify_tx, notify::Config::default().with_poll_interval(Duration::from_secs(2))) {
            Ok(w) => w,
            Err(e) => { error!("File watcher init failed: {}", e); return; }
        };
        if let Err(e) = watcher.watch(&workspace, RecursiveMode::Recursive) {
            error!("File watcher start failed: {}", e);
            return;
        }
        info!("File watcher started on {}", workspace.display());

        // Debounce map
        let mut last_events: std::collections::HashMap<PathBuf, std::time::Instant> = std::collections::HashMap::new();
        let debounce = Duration::from_millis(300);

        loop {
            match notify_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(event)) => {
                    for path in &event.paths {
                        let rel = path.strip_prefix(&workspace)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        // Skip .trash and .being files
                        if rel.starts_with(".trash") || rel.contains(".being") {
                            continue;
                        }
                        // Debounce
                        let now = std::time::Instant::now();
                        if let Some(last) = last_events.get(path) {
                            if now.duration_since(*last) < debounce {
                                continue;
                            }
                        }
                        last_events.insert(path.clone(), now);

                        let event_type = match event.kind {
                            notify::EventKind::Create(_) => "file-created",
                            notify::EventKind::Modify(_) => "file-changed",
                            notify::EventKind::Remove(_) => "file-deleted",
                            _ => continue,
                        };
                        let _ = tx.send(FileEvent {
                            event_type: event_type.to_string(),
                            path: rel,
                        });
                    }
                }
                Ok(Err(e)) => warn!("Watch error: {}", e),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Cleanup old debounce entries
                    let cutoff = std::time::Instant::now() - Duration::from_secs(10);
                    last_events.retain(|_, t| *t > cutoff);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}
