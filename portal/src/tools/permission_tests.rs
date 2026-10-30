use super::*;
use crate::tools::{ToolHost, client::ClientHandler};
use serde_json::json;
use std::{future::Future, pin::Pin, sync::{Arc, Mutex}};

struct Fixture(PathBuf);
impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
fn fixture(mode: PermissionMode) -> (Fixture, PortalConfig) {
    let root = std::env::temp_dir().join(format!("portal-permissions-{}", uuid::Uuid::new_v4()));
    let mut config = PortalConfig::default();
    config.security.workspace_root = root.join("workspace");
    config.security.permission_mode = mode;
    config.kits_enabled = false;
    config.subagent.enabled = false;
    config.prepare_workspace().unwrap();
    (Fixture(root), config)
}
struct Approver(Mutex<Vec<Value>>, bool);
impl ClientHandler for Approver {
    fn handle_client_command<'a>(&'a self, verb: &'a str, args: &'a str, _scene: Option<&'a str>)
        -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(verb, "security-approve");
            self.0.lock().unwrap().push(serde_json::from_str(args).unwrap());
            Ok(if self.1 { "allow-once" } else { "deny" }.into())
        })
    }
}

#[tokio::test]
async fn external_writes_require_exact_single_call_approval_and_reads_do_not() {
    for mode in [PermissionMode::Supervised, PermissionMode::AutoReview] {
        let (f, config) = fixture(mode);
        let outside = f.0.join("external.txt");
        std::fs::write(&outside, "original").unwrap();
        let host = ToolHost::new(&config);
        assert!(host.call("portal_file_read", json!({"path":outside})).await.unwrap().to_string().contains("original"));
        let edit = json!({"path":outside,"old_text":"original","new_text":"edited", "approved":true});
        assert!(host.call("portal_file_edit", edit.clone()).await.is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
        let approver = Arc::new(Approver(Mutex::new(vec![]), true));
        let approved = ToolHost::new(&config).with_client_handler(approver.clone());
        approved.call("portal_file_edit", edit).await.unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "edited");
        let request = approver.0.lock().unwrap()[0].clone();
        assert_eq!(request["isolation"], "file-scope");
        assert_eq!(request["target"], serde_json::to_value(outside.canonicalize().unwrap()).unwrap());
        assert!(host.call("portal_file_write", json!({"path":outside,"content":"no grant carried over"})).await.is_err());
        approved.call("portal_file_write", json!({"path":outside,"content":"second grant"})).await.unwrap();
        assert_eq!(approver.0.lock().unwrap().len(), 2);
        let nested = f.0.join("new-directory/file.txt");
        assert!(host.call("portal_file_write", json!({"path":nested,"content":"denied"})).await.is_err());
        assert!(!nested.parent().unwrap().exists(), "Denied requests cannot create parents");
        approved.call("portal_file_write", json!({"path":nested,"content":"allowed"})).await.unwrap();
        assert_eq!(std::fs::read_to_string(nested).unwrap(), "allowed");
    }
}

#[tokio::test]
async fn full_access_applies_to_all_file_tools_without_workspace_boundary() {
    let (f, config) = fixture(PermissionMode::FullAccess);
    let host = ToolHost::new(&config);
    let outside = f.0.join("other/note.txt");
    host.call("portal_file_write", json!({"path":outside,"content":"marker"})).await.unwrap();
    host.call("portal_file_edit", json!({"path":outside,"old_text":"marker","new_text":"edited marker"})).await.unwrap();
    assert!(host.call("portal_file_read", json!({"path":outside})).await.unwrap().to_string().contains("edited marker"));
    assert!(host.call("portal_file_list", json!({"path":outside.parent()})).await.unwrap().to_string().contains("note.txt"));
    assert!(host.call("portal_search", json!({"path":outside.parent(),"pattern":"marker"})).await.unwrap().to_string().contains("edited marker"));
}

#[tokio::test]
async fn secrets_ask_and_broad_search_cannot_disclose_secrets_or_hardlinks() {
    let (f, config) = fixture(PermissionMode::Supervised);
    let root = &config.security.workspace_root;
    std::fs::write(root.join(".env"), "SECRET_MARKER").unwrap();
    std::fs::write(f.0.join("private.txt"), "HARDLINK_MARKER").unwrap();
    std::fs::hard_link(f.0.join("private.txt"), root.join("alias.txt")).unwrap();
    std::fs::write(root.join("normal.txt"), "NORMAL_MARKER").unwrap();
    let host = ToolHost::new(&config);
    assert!(host.call("portal_file_read", json!({"path":".env"})).await.is_err());
    assert!(host.call("portal_file_read", json!({"path":"alias.txt"})).await.is_err());
    let result = host.call("portal_search", json!({"pattern":"MARKER","include_ignored":true})).await.unwrap().to_string();
    assert!(result.contains("NORMAL_MARKER"));
    assert!(!result.contains("SECRET_MARKER") && !result.contains("HARDLINK_MARKER"));
    let approver = Arc::new(Approver(Mutex::new(vec![]), true));
    let approved = ToolHost::new(&config).with_client_handler(approver.clone());
    assert!(approved.call("portal_file_read", json!({"path":".env"})).await.unwrap().to_string().contains("SECRET_MARKER"));
    assert!(approved.call("portal_search", json!({"path":".env","pattern":"MARKER"})).await.unwrap().to_string().contains("SECRET_MARKER"));
    assert_eq!(approver.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn secret_reads_edits_and_writes_share_the_same_approval_boundary() {
    for mode in [PermissionMode::Supervised, PermissionMode::AutoReview] {
        let (_f, config) = fixture(mode);
        let denied = ToolHost::new(&config);
        let approver = Arc::new(Approver(Mutex::new(vec![]), true));
        let approved = ToolHost::new(&config).with_client_handler(approver.clone());
        for path in [".aws/credentials", ".azure/accessTokens.json", ".gnupg/private.key", "auth.json", ".ENV.local"] {
            let target = config.security.workspace_root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, "DUMMY_CREDENTIAL").unwrap();
            for (tool, args) in [
                ("portal_file_read", json!({"path":path})),
                ("portal_file_edit", json!({"path":path,"old_text":"DUMMY_","new_text":"DUMMY_"})),
                ("portal_file_write", json!({"path":path,"content":"changed"})),
            ] {
                assert!(denied.call(tool, args.clone()).await.is_err(), "{tool}: {path}");
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "DUMMY_CREDENTIAL");
                let before = approver.0.lock().unwrap().len();
                approved.call(tool, args).await.unwrap();
                assert_eq!(approver.0.lock().unwrap().len(), before + 1);
            }
        }
    }
}

#[tokio::test]
async fn protected_configuration_requires_explicit_approval_and_disabled_tools_stay_disabled() {
    let (_f, mut config) = fixture(PermissionMode::Supervised);
    let approver = Arc::new(Approver(Mutex::new(vec![]), true));
    ToolHost::new(&config).with_client_handler(approver.clone())
        .call("portal_file_write", json!({"path":"tools/mcp.toml","content":"# approved"})).await.unwrap();
    assert_eq!(approver.0.lock().unwrap().len(), 1);
    config.tools.file = false;
    let host = ToolHost::new(&config).with_client_handler(approver.clone());
    assert!(host.call("portal_file_write", json!({"path":"notes.txt","content":"no"})).await.is_err());
    assert_eq!(approver.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn restricted_startup_never_loads_workspace_mcp() {
    for mode in [PermissionMode::ReadOnly, PermissionMode::Supervised, PermissionMode::AutoReview] {
        let (_f, config) = fixture(mode);
        std::fs::create_dir_all(config.security.workspace_root.join("tools")).unwrap();
        // Invalid config proves the restricted startup never even attempts discovery.
        std::fs::write(config.security.workspace_root.join("tools/mcp.toml"), "invalid toml = [").unwrap();
        let host = ToolHost::new(&config);
        assert_eq!(host.load_custom_tools().await.unwrap(), 0);
        assert!(host.call("portal_tools_reload", json!({})).await.is_err());
    }
}

#[test]
fn shell_escalation_is_explicit_and_never_creates_persistent_host_sessions() {
    let (_f, config) = fixture(PermissionMode::Supervised);
    assert_eq!(plan(&config, "portal_exec", &json!({"command":"echo ordinary"})).unwrap().decision, Decision::Allow);
    assert_eq!(plan(&config, "portal_process", &json!({"action":"poll"})).unwrap().decision, Decision::Allow);
    let args = json!({"command":"echo outside", "sandbox_permissions":"require_escalated", "justification":"needs host tools"});
    let host = plan(&config, "portal_exec", &args).unwrap();
    assert_eq!(host.decision, Decision::Ask);
    assert!(host.host_command);
    let mut background = args.clone(); background["background"] = json!(true);
    assert!(plan(&config, "portal_exec", &background).is_err());
    let mut no_reason = args; no_reason["justification"] = json!("");
    assert!(plan(&config, "portal_exec", &no_reason).is_err());
    assert!(plan(&config, "portal_exec", &json!({"command":"echo", "sandbox_permissions":"approve-me"})).is_err());
}
