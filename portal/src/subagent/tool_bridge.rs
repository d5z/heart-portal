//! A task-scoped, authenticated tool broker. Pi has no built-in tools; the only
//! registered tools forward to ToolHost, preserving policy, approvals and sandboxing.
use anyhow::Result;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, net::TcpListener, task::{JoinHandle, JoinSet}};
use crate::{config::PortalConfig, tools::{ToolHost, client::ClientHandler}};
use subtle::ConstantTimeEq;

const ALLOWED: &[&str] = &["portal_exec", "portal_process", "portal_file_read", "portal_file_write", "portal_file_edit", "portal_file_list", "portal_search", "portal_web_fetch"];
pub struct ToolBridge { pub port: u16, pub token: String, pub extension: PathBuf, task: JoinHandle<()>, host: ToolHost }
impl ToolBridge {
    pub fn start(mut config: PortalConfig, handler: Arc<dyn ClientHandler>, scene: Option<String>, directory: PathBuf)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self>> + Send>> {
      Box::pin(async move {
        config.subagent.enabled = false;
        config.subagent.auto_install = false;
        config.kits_enabled = false;
        config.tools.custom_tools_enabled = false;
        let runtime = crate::tools::status::RuntimeStatus::for_bridge(&config);
        let host = ToolHost::new_with_runtime(&config, runtime).with_client_handler(handler);
        let definitions: Vec<_> = host.list_tools().await.into_iter().filter(|t| ALLOWED.contains(&t.name.as_str()))
            .map(|t| json!({"name":t.name,"description":t.description,"inputSchema":t.input_schema})).collect();
        tokio::fs::create_dir_all(&directory).await?;
        let extension = directory.join(format!("tool-bridge-{}.mjs", uuid::Uuid::new_v4()));
        let source = include_str!("tool_bridge.mjs").replace("/*PORTAL_TOOL_DEFINITIONS*/[]", &serde_json::to_string(&definitions)?);
        tokio::fs::write(&extension, source).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let expected = token.clone();
        let cleanup_host = host.clone();
        let task = tokio::spawn(async move {
            let mut calls = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let Ok((stream, _)) = connection else { break; };
                        if calls.len() >= 4 { drop(stream); continue; }
                        let host = host.clone(); let token = expected.clone(); let scene = scene.clone();
                        calls.spawn(async move {
                            let (read, mut write) = stream.into_split();
                            let mut reader = BufReader::new(read);
                            let mut line = String::new();
                            let parsed = tokio::time::timeout(std::time::Duration::from_secs(5),
                                (&mut reader).take(1_048_577).read_line(&mut line)).await;
                            if !matches!(parsed, Ok(Ok(_))) || line.len() > 1_048_576 || !line.ends_with('\n') { return; }
                            let value: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => return };
                            let provided = value["token"].as_str().unwrap_or("");
                            if !bool::from(provided.as_bytes().ct_eq(token.as_bytes())) { return; }
                            let name = value["tool"].as_str().unwrap_or("");
                            if !ALLOWED.contains(&name) || !value["arguments"].is_object() { return; }
                            let mut closed = [0];
                            let result = tokio::select! {
                                result = host.call_with_scene(name, value["arguments"].clone(), scene.as_deref()) => result,
                                _ = reader.read(&mut closed) => return,
                            };
                            let value = result.unwrap_or_else(|e| json!({"isError":true,"content":[{"type":"text","text":e.to_string()}]}));
                            if let Ok(mut bytes) = serde_json::to_vec(&value) { bytes.push(b'\n'); let _ = write.write_all(&bytes).await; }
                        });
                    }
                    _ = calls.join_next(), if !calls.is_empty() => {}
                }
            }
        });
        Ok(Self { port, token, extension, task, host: cleanup_host })
      })
    }
}
impl Drop for ToolBridge {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.extension);
        let host = self.host.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { host.process_manager.kill_all().await; });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PermissionMode, tools::client::NoClientHandler};
    async fn request(bridge: &ToolBridge, token: &str, tool: &str, args: Value) -> Option<Value> {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", bridge.port)).await.unwrap();
        stream.write_all(format!("{}\n", json!({"token":token,"tool":tool,"arguments":args})).as_bytes()).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut response)).await.unwrap().unwrap();
        serde_json::from_str(&response).ok()
    }
    #[tokio::test]
    async fn broker_enforces_identity_policy_and_single_tool_allowlist() {
        let root = std::env::temp_dir().join(format!("portal-bridge-test-{}", uuid::Uuid::new_v4()));
        let mut config = PortalConfig::default();
        config.security.workspace_root = root.join("workspace");
        config.prepare_workspace().unwrap();
        config.security.permission_mode = PermissionMode::Supervised;
        let bridge = ToolBridge::start(config.clone(), Arc::new(NoClientHandler), Some("child-task".into()), root.join("bridge")).await.unwrap();
        let args = json!({"path":"notes.txt", "content":"child"});
        assert!(request(&bridge,"forged","portal_file_write", args.clone()).await.is_none());
        assert!(!config.security.workspace_root.join("notes.txt").exists());
        let result = request(&bridge,&bridge.token,"portal_file_write", args.clone()).await.unwrap();
        assert_ne!(result["isError"], true);
        assert_eq!(std::fs::read_to_string(config.security.workspace_root.join("notes.txt")).unwrap(),"child");
        let result = request(&bridge,&bridge.token,"portal_exec",json!({"command":"echo forbidden", "sandbox_permissions":"require_escalated", "justification":"test host operation", "approved":true})).await.unwrap();
        assert_eq!(result["isError"],true);
        assert!(request(&bridge,&bridge.token,"portal_subagent_spawn",json!({})).await.is_none());
        let extension = bridge.extension.clone(); drop(bridge);
        assert!(!extension.exists());
        config.security.permission_mode = PermissionMode::ReadOnly;
        let bridge = ToolBridge::start(config.clone(), Arc::new(NoClientHandler), None,root.join("bridge")).await.unwrap();
        assert_eq!(request(&bridge,&bridge.token,"portal_file_write",args).await.unwrap()["isError"],true);
        assert_ne!(request(&bridge,&bridge.token,"portal_file_read",json!({"path":"notes.txt"})).await.unwrap()["isError"],true);
        drop(bridge);
        tokio::task::yield_now().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
