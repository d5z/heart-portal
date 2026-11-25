//! Desktop Work uses the same ToolHost, session ledger, policy and callbacks as Being tools.
use super::{ledger::{LedgerTaskStatus, TaskRecord}, SubagentManager};
use crate::tools::ToolHost;
use anyhow::{Context, Result};
use axum::{extract::{Path, State}, http::{HeaderMap, StatusCode}, routing::post, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{fs::{self, OpenOptions}, io::{ErrorKind, Write}, path::{Path as FsPath, PathBuf}, sync::Arc};
use subtle::ConstantTimeEq;

impl SubagentManager {
    pub(crate) async fn retry_work(self: &Arc<Self>, id: &str) -> Result<Value> {
        let task = self.ledger.lock().await.task(id).context("任务不存在或已过期")?.clone();
        anyhow::ensure!(!task.status().is_open() && task.status() != LedgerTaskStatus::Done, "只能重试未成功结束的任务");
        let file = self.config.resolved_state_dir().join(format!("task-{id}.jsonl"));
        let text = tokio::fs::read_to_string(file).await.context("无法读取原始运行指令")?;
        let original = text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|row| row["kind"] == "started").context("找不到原始运行指令")?;
        let message = task.user_message.as_deref().or_else(|| original["input"].as_str()).context("找不到原始运行指令")?;
        self.continue_work(id, message).await
    }

    pub async fn continue_work(self: &Arc<Self>, id: &str, message: &str) -> Result<Value> {
        let (parent, brief) = self.continuation(id, message).await?;
        let receipt = self.spawn_work(super::SpawnRequest {
            brief, session: parent.session.clone(), workdir: Some(parent.workdir.clone()), scene_id: parent.scene_id.clone(),
            budget: self.default_budget(), model: None, thinking: None,
        }, Some((&parent, message))).await?;
        Ok(receipt.to_json())
    }
    async fn continuation(&self, id: &str, message: &str) -> Result<(TaskRecord, String)> {
        anyhow::ensure!(!message.trim().is_empty() && message.len() <= 16_000, "请输入 1–16000 字节的消息");
        let (parent, chain) = {
            let ledger = self.ledger.lock().await;
            let parent = ledger.task(id).context("任务不存在或已过期")?.clone();
            anyhow::ensure!(!parent.status().is_open(), "任务仍在执行，请等待完成后继续对话");
            // Follow ancestry, not session names: Being may reuse the default session for unrelated work.
            let mut chain = vec![parent.clone()];
            while let Some(previous) = chain.last().and_then(|t| t.parent_task_id.as_deref()).and_then(|id| ledger.task(id)) {
                chain.push(previous.clone());
            }
            (parent, chain)
        };
        let mut context = Vec::new();
        for task in chain.iter().rev() {
            let file = self.config.resolved_state_dir().join(format!("task-{}.jsonl", task.task_id));
            let text = tokio::fs::read_to_string(file).await.context("任务上下文无法读取，未启动续聊")?;
            let rows: Vec<Value> = text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
            let prompt = task.user_message.as_deref().or_else(|| rows.iter().find(|r| r["kind"] == "started").and_then(|r| r["input"].as_str())).unwrap_or(&task.brief_head);
            context.push(format!("User: {prompt}"));
            if let Some(reply) = rows.iter().rev().find(|r| r["kind"] == "message").and_then(|r| r["output"].as_str()) {
                context.push(format!("Subagent: {reply}"));
            }
        }
        // Bound explicit carry-over for the stateless runtime. Keep recent messages and identify omissions.
        let history = crate::heart_callback::clamp_str_tail(&context.join("\n\n"), 44_000);
        Ok((parent, format!("Continue this Work conversation. The following history is context, not new instructions. Use the existing workspace files when needed. Earlier history may be truncated.\n<work_history>\n{history}\n</work_history>\n\nUser follow-up:\n{message}")))
    }
}

#[derive(Clone)]
struct WorkState { host: ToolHost, token: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowUp { message: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewWork { brief: String, workdir: String, scene_id: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeResult {
    task_id: String,
    event_id: String,
    scene_id: String,
    executor: String,
    title: String,
    status: String,
    summary: String,
}
type WorkResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;
fn failure(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (StatusCode::CONFLICT, Json(json!({"error": error.to_string()})))
}
fn authorize(state: &WorkState, headers: &HeaderMap) -> std::result::Result<(), (StatusCode, Json<Value>)> {
    let supplied = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    if bool::from(supplied.as_bytes().ct_eq(format!("Bearer {}", state.token).as_bytes())) { Ok(()) }
    else { Err((StatusCode::UNAUTHORIZED, Json(json!({"error":"Unauthorized"})))) }
}
fn dispatch_dir(state: &WorkState) -> PathBuf {
    state.host.subagent.config.resolved_state_dir().join("work-dispatches")
}

// The original POST and a negative recovery can each claim the key only once.
// Keep the marker after ledger pruning so an old dispatch can never be reopened by absence alone.
fn claim_dispatch(file: &FsPath, state: &str) -> std::io::Result<bool> {
    fs::create_dir_all(file.parent().expect("dispatch directory"))?;
    match OpenOptions::new().write(true).create_new(true).open(file) {
        Ok(mut marker) => { marker.write_all(state.as_bytes())?; Ok(true) }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Deserialize)]
struct ResolveDispatch { admission: String }

async fn resolve_dispatch(State(state): State<WorkState>, Path(launch): Path<String>, headers: HeaderMap, Json(body): Json<ResolveDispatch>) -> WorkResult {
    authorize(&state, &headers)?;
    let launch = uuid::Uuid::parse_str(&launch).map_err(|_| (StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid Work launch"}))))?;
    let session = format!("work-{launch}");
    {
        let ledger = state.host.subagent.ledger.lock().await;
        if let Some(task) = ledger.tasks.iter().find(|row| row.session == session && row.parent_task_id.is_none()) {
            return Ok(Json(json!({"status":"started", "task_id":task.task_id})));
        }
    }
    let directory = dispatch_dir(&state);
    if fs::read_to_string(directory.join(".epoch")).map_err(failure)? != body.admission {
        return Ok(Json(json!({"status":"unconfirmed"})));
    }
    let file = directory.join(launch.to_string());
    let retired = claim_dispatch(&file, "retired").map_err(failure)? || fs::read_to_string(&file).map_err(failure)? == "retired";
    Ok(Json(json!({"status": if retired { "not_accepted" } else { "unconfirmed" }})))
}

async fn create(State(state): State<WorkState>, headers: HeaderMap, Json(body): Json<NewWork>) -> WorkResult {
    authorize(&state, &headers)?;
    let (launch, file) = match headers.get("x-work-launch") {
        Some(value) => {
            let launch = value.to_str().ok().and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid Work launch"}))))?;
            let file = dispatch_dir(&state).join(launch.to_string());
            if !claim_dispatch(&file, "admitted").map_err(failure)? {
                let session = format!("work-{launch}");
                let ledger = state.host.subagent.ledger.lock().await;
                if let Some(task) = ledger.tasks.iter().find(|row| row.session == session && row.parent_task_id.is_none()) {
                    return Ok(Json(json!({"task_id":task.task_id, "session":session, "status":task.status()})));
                }
                if fs::read_to_string(&file).is_ok_and(|phase| phase == "retired") {
                    return Err((StatusCode::CONFLICT, Json(json!({"error":"Work dispatch was already retired"}))));
                }
                return Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"Work dispatch was already admitted; check its original receipt"}))));
            }
            (launch, Some(file))
        }
        None => (uuid::Uuid::new_v4(), None),
    };
    let reply = state.host.call_with_scene("portal_subagent_spawn", json!({"brief":body.brief, "workdir":body.workdir, "scene_id":body.scene_id, "session":format!("work-{launch}")}), Some(&body.scene_id)).await;
    let result = match reply {
        Ok(result) if result["isError"] != true => result,
        error => {
            if let Some(file) = file {
                if let Err(error) = fs::write(file, "retired") { tracing::warn!(%error, "Could not record Work dispatch rejection"); }
            }
            return match error { Ok(result) => Err((StatusCode::CONFLICT, Json(result))), Err(error) => Err(failure(error)) };
        }
    };
    let receipt: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap_or("")).map_err(failure)?;
    Ok(Json(receipt))
}
async fn follow_up(State(state): State<WorkState>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<FollowUp>) -> WorkResult {
    authorize(&state, &headers)?;
    let result = state.host.call_with_scene("portal_subagent_control", json!({"action":"continue", "task_id":id, "message":body.message}), None).await.map_err(failure)?;
    if result["isError"] == true { return Err((StatusCode::CONFLICT, Json(result))); }
    let receipt: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap_or("")).map_err(failure)?;
    Ok(Json(receipt))
}

async fn cancel(State(state): State<WorkState>, Path(id): Path<String>, headers: HeaderMap) -> WorkResult {
    authorize(&state, &headers)?;
    let result = state.host.call_with_scene("portal_subagent_control", json!({"action":"cancel", "task_id":id}), None).await.map_err(failure)?;
    if result["isError"] == true { Err((StatusCode::CONFLICT, Json(result))) } else { Ok(Json(result)) }
}
async fn steer(State(state): State<WorkState>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<FollowUp>) -> WorkResult {
    authorize(&state, &headers)?;
    let result = state.host.call_with_scene("portal_subagent_control", json!({"action":"steer", "task_id":id, "message":body.message}), None).await.map_err(failure)?;
    if result["isError"] == true { Err((StatusCode::CONFLICT, Json(result))) } else { Ok(Json(result)) }
}
async fn retry(State(state): State<WorkState>, Path(id): Path<String>, headers: HeaderMap) -> WorkResult {
    authorize(&state, &headers)?;
    let result = state.host.call_with_scene("portal_subagent_control", json!({"action":"retry", "task_id":id}), None).await.map_err(failure)?;
    if result["isError"] == true { Err((StatusCode::CONFLICT, Json(result))) } else { Ok(Json(result)) }
}
async fn publish_result(State(state): State<WorkState>, headers: HeaderMap, Json(body): Json<NativeResult>) -> WorkResult {
    authorize(&state, &headers)?;
    if !matches!(body.executor.as_str(), "codex" | "deepseek") ||
        !body.task_id.starts_with(&format!("agent_{}_", body.executor)) ||
        !body.event_id.starts_with(&format!("{}:", body.task_id)) || body.event_id.len() > 320 ||
        body.scene_id.is_empty() || body.scene_id.len() > 256 || body.summary.len() > 8_000 ||
        !matches!(body.status.as_str(), "done" | "failed" | "interrupted" | "timeout" | "budget_exhausted")
    {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid Work result"}))));
    }
    let result = json!({
        "kind": "subagent", "task_id": body.task_id, "scene_id": body.scene_id, "executor": body.executor,
        "brief": body.title, "status": body.status, "result": body.summary, "event_id": body.event_id, "truncated": true,
    });
    state.host.deliver_work_result(body.event_id, result).await.map_err(failure)?;
    Ok(Json(json!({"delivered":true})))
}

/// Only client-managed engines publish this loopback capability; the renderer never sees it.
pub async fn start(host: ToolHost) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let (Some(ready), Ok(nonce)) = (std::env::var_os("HEART_PORTAL_READY_FILE"), std::env::var("HEART_PORTAL_READY_NONCE")) else { return Ok(None); };
    if std::env::var_os("HEART_PORTAL_CLIENT_FILE").is_none() { return Ok(None); }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let state = WorkState { host, token: uuid::Uuid::new_v4().to_string() };
    let token = state.token.clone();
    let epoch_file = dispatch_dir(&state).join(".epoch");
    let admission = match claim_dispatch(&epoch_file, &uuid::Uuid::new_v4().to_string()).and_then(|_| fs::read_to_string(epoch_file)) {
        Ok(admission) => Some(admission),
        Err(error) => { tracing::warn!(%error, "Work dispatch admission recovery is unavailable"); None }
    };
    let router = Router::new()
        .route("/api/subagent/tasks", post(create))
        .route("/api/subagent/dispatches/{launch}/resolve", post(resolve_dispatch))
        .route("/api/subagent/tasks/{id}/messages", post(follow_up))
        .route("/api/subagent/tasks/{id}/cancel", post(cancel))
        .route("/api/subagent/tasks/{id}/steer", post(steer))
        .route("/api/subagent/tasks/{id}/retry", post(retry))
        .route("/api/subagent/results", post(publish_result))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state);
    let file = PathBuf::from(ready).with_file_name(".portal-work.json");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    options.open(&file)?.write_all(serde_json::to_string(&json!({"pid":std::process::id(), "nonce":nonce, "port":port, "token":token, "launchAdmission":admission}))?.as_bytes())?;
    Ok(Some(tokio::spawn(async move { let _ = axum::serve(listener, router).await; })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::activity;
    use crate::{config::PortalConfig, heart_callback::HeartCallback, subagent::ledger::LedgerTaskStatus};

    #[cfg(unix)]
    #[tokio::test]
    async fn new_work_keeps_the_main_scene_and_uses_distinct_sessions() {
        let root = PathBuf::from("/tmp").join(format!("wc-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = PortalConfig::default();
        config.kits_enabled = false;
        config.security.workspace_root = root.clone();
        config.subagent.state_dir = Some(root.join("state").display().to_string());
        config.subagent.command = Some(vec!["/usr/bin/true".into()]);
        config.subagent.model.provider = Some("openrouter".into());
        config.subagent.model.model = Some("test-model".into());
        config.subagent.model.api_key = Some("test-key".into());
        let state = WorkState { host: ToolHost::new(&config), token: "test-capability".into() };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-capability".parse().unwrap());
        let mut sessions = Vec::new();
        let launch = uuid::Uuid::new_v4();
        for (index, brief) in ["First independent task", "Second independent task"].into_iter().enumerate() {
            if index == 1 { headers.insert("x-work-launch", launch.to_string().parse().unwrap()); }
            let Json(receipt) = create(State(state.clone()), headers.clone(), Json(NewWork { brief: brief.into(), workdir: root.display().to_string(), scene_id: "desktop-main".into() })).await.unwrap();
            let ledger = state.host.subagent.ledger.lock().await;
            let task = ledger.task(receipt["task_id"].as_str().unwrap()).unwrap();
            assert_eq!(task.scene_id.as_deref(), Some("desktop-main"));
            assert!(task.parent_task_id.is_none());
            sessions.push(task.session.clone());
        }
        assert_ne!(sessions[0], sessions[1]);
        assert_eq!(sessions[1], format!("work-{launch}"));
        let epoch = "fixture-epoch";
        assert!(claim_dispatch(&dispatch_dir(&state).join(".epoch"), epoch).unwrap());
        let Json(accepted) = resolve_dispatch(State(state.clone()), Path(launch.to_string()), headers.clone(), Json(ResolveDispatch { admission: epoch.into() })).await.unwrap();
        assert_eq!(accepted["status"], "started");

        let absent = uuid::Uuid::new_v4();
        let Json(unknown) = resolve_dispatch(State(state.clone()), Path(absent.to_string()), headers.clone(), Json(ResolveDispatch { admission: "other-state".into() })).await.unwrap();
        assert_eq!(unknown["status"], "unconfirmed");
        let Json(retired) = resolve_dispatch(State(state.clone()), Path(absent.to_string()), headers.clone(), Json(ResolveDispatch { admission: epoch.into() })).await.unwrap();
        assert_eq!(retired["status"], "not_accepted");
        let repeat = |key: uuid::Uuid| {
            let mut headers = headers.clone();
            headers.insert("x-work-launch", key.to_string().parse().unwrap());
            create(State(state.clone()), headers, Json(NewWork { brief: "Repeated dispatch".into(), workdir: root.display().to_string(), scene_id: "desktop-main".into() }))
        };
        let Json(replayed) = repeat(launch).await.unwrap();
        assert_eq!(replayed["task_id"], accepted["task_id"]);
        assert_eq!(repeat(absent).await.unwrap_err().0, StatusCode::CONFLICT);
        let admitting = uuid::Uuid::new_v4();
        assert!(claim_dispatch(&dispatch_dir(&state).join(admitting.to_string()), "admitted").unwrap());
        assert_eq!(repeat(admitting).await.unwrap_err().0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state.host.subagent.ledger.lock().await.tasks.len(), 2);
        state.host.subagent.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn continuation_uses_task_ancestry_after_restart_and_rejects_running_tasks() {
        let root = std::env::temp_dir().join(format!("portal-work-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = PortalConfig::default();
        config.security.workspace_root = root.clone();
        config.subagent.state_dir = Some(root.display().to_string());
        let manager = SubagentManager::new(&config, HeartCallback::new());
        {
            let mut ledger = manager.ledger.lock().await;
            for (id, brief) in [("sub_first", "Original task"), ("sub_other", "Unrelated work"), ("sub_next", "Continuation"), ("sub_busy", "Running")] {
                ledger.start_task(id, "default", brief, root.to_str().unwrap(), Some("scene-main"));
                if id != "sub_busy" { ledger.finish_task(id, LedgerTaskStatus::Done, 1, 10, None); }
            }
            let next = ledger.tasks.iter_mut().find(|t| t.task_id == "sub_next").unwrap();
            next.parent_task_id = Some("sub_first".into());
            next.user_message = Some("Keep the original format".into());
            ledger.save().unwrap();
        }
        for (id, prompt, reply) in [("sub_first", "Original full instructions", "First result"), ("sub_next", "GENERATED CARRYOVER MUST NOT REPEAT", "Second result")] {
            activity::append(&root.join(format!("task-{id}.jsonl")), json!({"kind":"started", "input":prompt}));
            activity::append(&root.join(format!("task-{id}.jsonl")), json!({"kind":"message", "output":reply}));
        }
        drop(manager);
        let manager = SubagentManager::new(&config, HeartCallback::new());
        let (parent, prompt) = manager.continuation("sub_next", "Add an example").await.unwrap();
        assert_eq!(parent.scene_id.as_deref(), Some("scene-main"));
        for expected in ["Original full instructions", "First result", "Keep the original format", "Second result", "Add an example"] { assert!(prompt.contains(expected), "{prompt}"); }
        assert!(!prompt.contains("Unrelated"));
        assert!(!prompt.contains("GENERATED CARRYOVER"));
        assert!(manager.continuation("sub_busy", "new work").await.is_err());
        assert!(manager.continuation("sub_next", " ").await.is_err());
        assert!(manager.continuation("sub_missing", "new work").await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn desktop_controls_require_capability_and_preserve_tool_policy() {
        let config = PortalConfig::default();
        let state = WorkState { host: ToolHost::new(&config), token: "private-work-capability".into() };
        let denied = cancel(State(state.clone()), Path("sub_unknown".into()), HeaderMap::new()).await.unwrap_err();
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer private-work-capability".parse().unwrap());
        let denied = cancel(State(state), Path("sub_unknown".into()), headers).await.unwrap_err();
        assert_eq!(denied.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn native_result_uses_the_authenticated_heart_callback_and_confirms_delivery() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let app = Router::new().route("/api/callback", post(move |uri: axum::http::Uri, Json(body): Json<Value>| {
            let sender = sender.clone();
            async move {
                assert_eq!(uri.query(), Some("token=fixture-token"));
                sender.send(body).await.unwrap();
                StatusCode::ACCEPTED
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let host = ToolHost::new(&PortalConfig::default());
        host.set_callback_config(format!("http://{address}/api/callback"), "fixture-token".into(), "Test Portal".into());
        let state = WorkState { host, token: "private-work-capability".into() };
        let body = || NativeResult {
            task_id: "agent_codex_session".into(), event_id: "agent_codex_session:turn".into(),
            scene_id: "desktop-main".into(), executor: "codex".into(), title: "Work title".into(),
            status: "done".into(), summary: "Work result".into(),
        };
        let denied = publish_result(State(state.clone()), HeaderMap::new(), Json(body())).await.unwrap_err();
        assert_eq!(denied.0, StatusCode::UNAUTHORIZED);
        assert!(receiver.try_recv().is_err());
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer private-work-capability".parse().unwrap());
        let Json(receipt) = publish_result(State(state), headers, Json(body())).await.unwrap();
        assert_eq!(receipt["delivered"], true);
        let payload = receiver.try_recv().unwrap();
        assert_eq!(payload["task_id"], "agent_codex_session:turn");
        assert_eq!(payload["result"]["task_id"], "agent_codex_session");
        assert_eq!(payload["result"]["scene_id"], "desktop-main");
        assert_eq!(payload["result"]["result"], "Work result");
        assert_eq!(payload["result"]["portal_name"], "Test Portal");
        server.abort();
    }
}
