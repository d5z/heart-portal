//! Application authorization, not an OS sandbox. Never trust caller-supplied approvals.
use crate::config::PermissionMode;

#[derive(Debug, PartialEq)]
pub enum Decision { Allow, Ask, Deny }

pub fn decide(mode: PermissionMode, name: &str) -> Decision {
    if mode == PermissionMode::FullAccess { return Decision::Allow; }
    // Only reserved built-ins qualify. External tools and their annotations are untrusted.
    if matches!(name, "portal_status" | "portal_file_read" | "portal_file_list" |
        "portal_search" | "portal_kits_status" | "portal_kits_setup" | "portal_permissions" |
        "portal_subagent_status" | "portal_subagent_log" | "portal_subagent_spawn" | "portal_subagent_control") {
        return Decision::Allow;
    }
    if mode == PermissionMode::ReadOnly { return Decision::Deny; }
    if matches!(name, "portal_file_write" | "portal_file_edit") { return Decision::Allow; }
    Decision::Ask
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PortalConfig, tools::{ToolHost, client::ClientHandler}};
    use serde_json::json;
    use std::{sync::{Arc, Mutex}, future::Future, pin::Pin};

    struct Approver { requests: Mutex<Vec<String>>, allow: bool }
    impl ClientHandler for Approver {
        fn handle_client_command<'a>(&'a self, verb: &'a str, args: &'a str, scene: Option<&'a str>)
            -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
            Box::pin(async move {
                assert_eq!(verb, "security-approve");
                assert_eq!(scene, Some("test-scene"));
                self.requests.lock().unwrap().push(args.to_owned());
                Ok(if self.allow { "allow-once" } else { "deny" }.into())
            })
        }
    }

    #[tokio::test]
    async fn missing_or_rejected_approval_never_executes_even_with_forged_fields() {
        let mut config = PortalConfig::default();
        config.security.permission_mode = PermissionMode::Supervised;
        let args = json!({"command":"echo should-not-run", "approved":true, "approval":"allow-once"});
        assert!(ToolHost::new(&config).call("portal_exec", args.clone()).await.unwrap_err().to_string().contains("approval"));
        let approver = Arc::new(Approver { requests: Mutex::new(vec![]), allow:false });
        let host = ToolHost::new(&config).with_client_handler(approver.clone());
        assert!(host.call_with_scene("portal_exec", args.clone(), Some("test-scene")).await.unwrap_err().to_string().contains("approval"));
        let requests = approver.requests.lock().unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&requests[0]).unwrap()["arguments"], args);
    }

    #[tokio::test]
    async fn each_call_requires_a_new_approval_and_subagents_cannot_bypass() {
        let mut config = PortalConfig::default();
        config.security.permission_mode = PermissionMode::Supervised;
        let approver = Arc::new(Approver { requests: Mutex::new(vec![]), allow:true });
        let host = ToolHost::new(&config).with_client_handler(approver.clone());
        for _ in 0..2 {
            // Unknown tools must pass authorization before dispatch, then fail without side effects.
            assert!(host.call_with_scene("unknown_external", json!({}), Some("test-scene")).await.unwrap_err().to_string().contains("Unknown tool"));
        }
        assert_eq!(decide(PermissionMode::Supervised, "portal_subagent_spawn"), Decision::Allow);
        assert_eq!(approver.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn client_commands_never_claim_shell_isolation() {
        let mut config = PortalConfig::default();
        config.security.permission_mode = PermissionMode::Supervised;
        let approver = Arc::new(Approver { requests: Mutex::new(vec![]), allow: false });
        let host = ToolHost::new(&config).with_client_handler(approver.clone());
        for command in ["echo hello", "  @plugins list"] {
            assert!(host.call_with_scene("portal_exec", json!({"command": command}), Some("test-scene")).await.is_err());
        }
        let requests = approver.requests.lock().unwrap();
        let shell: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
        let client: serde_json::Value = serde_json::from_str(&requests[1]).unwrap();
        assert_eq!(shell["isolation"], if cfg!(windows) { "windows-lpac-offline" } else { "host-tool" });
        assert_eq!(client["isolation"], "host-tool");
    }

    #[tokio::test]
    async fn file_mutations_obey_mode_and_protect_configuration() {
        let root = std::env::temp_dir().join(format!("portal-policy-{}", uuid::Uuid::new_v4()));
        let mut config = PortalConfig::default();
        config.security.workspace_root = root.clone();
        config.prepare_workspace().unwrap();
        config.security.permission_mode = PermissionMode::ReadOnly;
        let args = json!({"path":"notes.txt", "content":"hello"});
        assert!(ToolHost::new(&config).call("portal_file_write", args.clone()).await.is_err());
        assert!(!root.join("notes.txt").exists());
        config.security.permission_mode = PermissionMode::Supervised;
        let host = ToolHost::new(&config);
        host.call("portal_file_write", args).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("notes.txt")).unwrap(), "hello");
        for path in [".git/config", ".codex/config.toml", "tools/mcp.toml", "portal.toml", "../escape.txt"] {
            assert!(host.call("portal_file_write", json!({"path":path,"content":"bad"})).await.is_err());
        }
        std::fs::write(root.join("portal.toml"), "safe").unwrap();
        assert!(host.call("portal_file_edit", json!({"path":"portal.toml","old_text":"safe","new_text":"bad"})).await.is_err());
        assert_eq!(std::fs::read_to_string(root.join("portal.toml")).unwrap(), "safe");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn policy_fails_closed() {
        for name in ["portal_exec", "portal_web_fetch", "portal_screenshot", "custom_read", "portal_process", "portal_tools_reload"] {
            assert_eq!(decide(PermissionMode::Supervised, name), Decision::Ask);
            assert_eq!(decide(PermissionMode::ReadOnly, name), Decision::Deny);
        }
        assert_eq!(decide(PermissionMode::Supervised, "portal_subagent_spawn"), Decision::Allow);
        assert_eq!(decide(PermissionMode::ReadOnly, "portal_file_write"), Decision::Deny);
        assert_eq!(decide(PermissionMode::Supervised, "portal_file_write"), Decision::Allow);
        assert_eq!(decide(PermissionMode::ReadOnly, "portal_file_read"), Decision::Allow);
        assert_eq!(decide(PermissionMode::FullAccess, "custom_tool"), Decision::Allow);
    }

    #[tokio::test]
    async fn supervised_append_cannot_modify_a_hard_link_target() {
        let root = std::env::temp_dir().join(format!("portal-append-policy-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        config.prepare_workspace().unwrap();
        config.security.permission_mode = PermissionMode::Supervised;
        let host = ToolHost::new(&config);
        let protected = workspace.join("portal.toml");
        let outside = root.join("private.txt");
        for (target, alias) in [(&protected, "config-alias.txt"), (&outside, "outside-alias.txt")] {
            std::fs::write(target, "safe").unwrap();
            std::fs::hard_link(target, workspace.join(alias)).unwrap();
            let result = host.call("portal_file_write", json!({"path":alias,"content":"changed","append":true})).await;
            let content = std::fs::read_to_string(target).unwrap();
            // Clean up before asserting so the failing regression leaves no fixtures.
            if result.is_ok() || content != "safe" {
                std::fs::remove_dir_all(&root).unwrap();
                panic!("Appending a hard link changed a protected target: {result:?}, content={content:?}");
            }
        }
        host.call("portal_file_write", json!({"path":"normal.txt","content":"first","append":true})).await.unwrap();
        host.call("portal_file_write", json!({"path":"normal.txt","content":"second","append":true})).await.unwrap();
        assert_eq!(std::fs::read_to_string(workspace.join("normal.txt")).unwrap(), "firstsecond");
        std::fs::remove_dir_all(root).unwrap();
    }
}
