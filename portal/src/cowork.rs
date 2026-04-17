//! Cowork — HTTP/WS server for being's home UI.
//!
//! Serves the Cowork single-page app, file CRUD, and WebSocket file-watch.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Query, State, ws};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::Multipart;
use futures_util::{SinkExt, StreamExt};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tracing::{info, warn, error};

use crate::config::PortalConfig;

static COWORK_HTML: &str = include_str!("../cowork.html");

// --- Shared state ---

#[derive(Clone)]
pub struct CoworkState {
    pub config: PortalConfig,
    pub workspace: PathBuf,
    pub file_events: broadcast::Sender<FileEvent>,
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

/// Cowork bearer token from `PORTAL_TOKEN` / `LOOM_TOKEN` (startup warning + shared helper).
pub fn cowork_token() -> Option<String> {
    let t = std::env::var("PORTAL_TOKEN").unwrap_or_default();
    if !t.is_empty() { return Some(t); }
    let t = std::env::var("LOOM_TOKEN").unwrap_or_default();
    if !t.is_empty() { return Some(t); }
    None
}

fn check_auth(headers: &HeaderMap) -> Result<(), StatusCode> {
    let token = std::env::var("PORTAL_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return Ok(());
    }
    let auth = headers.get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth == format!("Bearer {}", token) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

// --- Path safety ---

/// Strip Windows `\\?\` UNC prefix for consistent path comparison.
#[cfg(windows)]
fn strip_unc_prefix(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        std::path::PathBuf::from(stripped)
    } else {
        p
    }
}
#[cfg(not(windows))]
fn strip_unc_prefix(p: std::path::PathBuf) -> std::path::PathBuf { p }

fn safe_path(workspace: &Path, rel: &str) -> Result<PathBuf, StatusCode> {
    if rel.is_empty() || rel == "." {
        return Ok(workspace.to_path_buf());
    }
    let joined = workspace.join(rel);
    // For non-existent paths, check parent
    let check = if joined.exists() {
        strip_unc_prefix(joined.canonicalize().map_err(|_| StatusCode::BAD_REQUEST)?)
    } else {
        let parent = joined.parent().ok_or(StatusCode::BAD_REQUEST)?;
        if !parent.exists() {
            return Err(StatusCode::NOT_FOUND);
        }
        let canon_parent = strip_unc_prefix(parent.canonicalize().map_err(|_| StatusCode::BAD_REQUEST)?);
        canon_parent.join(joined.file_name().ok_or(StatusCode::BAD_REQUEST)?)
    };
    let ws_canon = strip_unc_prefix(workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf()));
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
        .route("/api/rename", post(api_rename))
        .route("/ws", get(ws_handler))
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

async fn api_soul(State(st): State<CoworkState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": st.config.name,
        "version": env!("CARGO_PKG_VERSION")
    }))
}

async fn api_files(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Result<Json<Vec<FileEntry>>, StatusCode> {
    check_auth(&headers)?;
    let base = safe_path(&st.workspace, q.path.as_deref().unwrap_or(""))?;
    let entries = list_dir_recursive(&base, &st.workspace, 3).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(entries))
}

fn list_dir_recursive(dir: &Path, workspace: &Path, depth: u32) -> std::io::Result<Vec<FileEntry>> {
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
            Some(list_dir_recursive(&e.path(), workspace, depth - 1)?)
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
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "pdf" | "mp3" | "wav" | "ogg" | "m4a" | "mp4" | "webm" => {
            let ct = match ext {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "svg" => "image/svg+xml",
                "webp" => "image/webp",
                "pdf" => "application/pdf",
                "mp3" => "audio/mpeg",
                "wav" => "audio/wav",
                "ogg" => "audio/ogg",
                "m4a" => "audio/mp4",
                "mp4" => "video/mp4",
                "webm" => "video/webm",
                _ => "application/octet-stream",
            };
            let data = tokio::fs::read(&path).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Response::builder()
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(data))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
        }
        _ => {
            let content = tokio::fs::read_to_string(&path).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Response::builder()
                .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(Body::from(content))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
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
    let fname = path.file_name().ok_or(StatusCode::BAD_REQUEST)?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let dest = trash.join(format!("{}.{}", fname.to_string_lossy(), ts));
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

// --- Rename ---

#[derive(Deserialize)]
struct RenameReq {
    from: String,
    to: String,
}

async fn api_rename(
    State(st): State<CoworkState>,
    headers: HeaderMap,
    Json(body): Json<RenameReq>,
) -> Result<StatusCode, StatusCode> {
    check_auth(&headers)?;
    let src = safe_path(&st.workspace, &body.from)?;
    let dst = safe_path(&st.workspace, &body.to)?;
    if !src.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    if dst.exists() {
        return Err(StatusCode::CONFLICT);
    }
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    tokio::fs::rename(&src, &dst).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

// --- WebSocket ---

async fn ws_handler(
    State(st): State<CoworkState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    upgrade: ws::WebSocketUpgrade,
) -> Result<impl IntoResponse, StatusCode> {
    // Check token auth for WebSocket via query param
    let token = std::env::var("PORTAL_TOKEN").unwrap_or_default();
    if !token.is_empty() {
        let provided = q.get("token").map(|s| s.as_str()).unwrap_or("");
        if provided != token {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(upgrade.on_upgrade(move |socket| handle_ws(socket, st)))
}

async fn handle_ws(socket: ws::WebSocket, st: CoworkState) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = st.file_events.subscribe();

    // Forward file events to client
    let send_task = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            let msg = serde_json::to_string(&event).unwrap_or_default();
            if sender.send(ws::Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Handle incoming (heartbeat pongs, etc.)
    let recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(_) => {} // keep alive
                Err(_) => break,
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
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
                        if rel.starts_with(".trash") || rel.starts_with(".being") {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::{Mutex, OnceLock};
    use tower::ServiceExt;

    /// Serialize tests that mutate `PORTAL_TOKEN` / `LOOM_TOKEN`.
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn temp_workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("heart_portal_cowork_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp workspace");
        dir
    }

    #[test]
    fn safe_path_root_and_empty() {
        let ws = temp_workspace();
        let root = safe_path(&ws, "").expect("root");
        assert_eq!(root, ws);

        let dot = safe_path(&ws, ".").expect("dot");
        assert_eq!(dot, root);
    }

    #[test]
    fn safe_path_nested_file_and_dir() {
        let ws = temp_workspace();
        let sub = ws.join("docs");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("readme.txt"), b"hi").unwrap();

        let p = safe_path(&ws, "docs/readme.txt").expect("nested file");
        assert!(p.is_file());

        let d = safe_path(&ws, "docs").expect("nested dir");
        assert!(d.is_dir());
    }

    #[test]
    fn safe_path_new_file_under_existing_parent() {
        let ws = temp_workspace();
        std::fs::create_dir_all(ws.join("a")).unwrap();
        let p = safe_path(&ws, "a/new.txt").expect("new path under existing parent");
        let expected = ws.join("a").canonicalize().unwrap().join("new.txt");
        assert_eq!(p, expected);
    }

    #[test]
    fn safe_path_parent_missing_is_not_found() {
        let ws = temp_workspace();
        let err = safe_path(&ws, "missing_parent/x.txt").unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[test]
    fn safe_path_traversal_is_forbidden() {
        let ws = temp_workspace();
        let err = safe_path(&ws, "..").unwrap_err();
        assert_eq!(err, StatusCode::FORBIDDEN);
    }

    #[test]
    fn list_dir_recursive_skips_hidden_sorts_dirs_first() {
        let ws = temp_workspace();
        std::fs::write(ws.join("zebra.txt"), b"z").unwrap();
        std::fs::create_dir(ws.join("folder")).unwrap();
        std::fs::write(ws.join("folder").join("inner.txt"), b"i").unwrap();
        std::fs::write(ws.join(".hidden"), b"h").unwrap();

        let entries = list_dir_recursive(&ws, &ws, 1).expect("list");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "folder");
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].name, "zebra.txt");
    }

    #[test]
    fn list_dir_recursive_non_dir_returns_empty() {
        let ws = temp_workspace();
        std::fs::write(ws.join("f.txt"), b"x").unwrap();
        let entries = list_dir_recursive(&ws.join("f.txt"), &ws, 1).expect("list file");
        assert!(entries.is_empty());
    }

    #[test]
    fn file_event_serializes_type_alias() {
        let ev = FileEvent {
            event_type: "file-changed".to_string(),
            path: "a/b".to_string(),
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v.get("type").and_then(|x| x.as_str()), Some("file-changed"));
        assert_eq!(v.get("path").and_then(|x| x.as_str()), Some("a/b"));
    }

    #[test]
    fn check_auth_no_token_allows_all() {
        let _g = env_lock();
        let old = std::env::var("PORTAL_TOKEN").ok();
        std::env::remove_var("PORTAL_TOKEN");
        let headers = HeaderMap::new();
        let r = check_auth(&headers);
        assert!(r.is_ok());
        restore_env("PORTAL_TOKEN", old.as_deref());
    }

    #[test]
    fn check_auth_requires_bearer_when_token_set() {
        let _g = env_lock();
        let old = std::env::var("PORTAL_TOKEN").ok();
        std::env::set_var("PORTAL_TOKEN", "secret-test-token");

        let mut bad = HeaderMap::new();
        bad.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert_eq!(check_auth(&bad), Err(StatusCode::UNAUTHORIZED));

        let mut good = HeaderMap::new();
        good.insert(header::AUTHORIZATION, "Bearer secret-test-token".parse().unwrap());
        assert!(check_auth(&good).is_ok());

        restore_env("PORTAL_TOKEN", old.as_deref());
    }

    fn restore_env(key: &str, value: Option<&str>) {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[tokio::test]
    async fn cowork_router_health_ok() {
        let ws = temp_workspace();
        let (tx, _) = broadcast::channel::<FileEvent>(4);
        let state = CoworkState {
            config: crate::config::PortalConfig::default(),
            workspace: ws,
            file_events: tx,
        };
        let app = cowork_router(state).into_service();

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("\"status\":\"ok\""));
        assert!(body.contains("\"name\":\"portal\""));
    }
}
