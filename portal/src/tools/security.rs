//! Application authorization, not an OS sandbox. Never trust caller-supplied approvals.
use crate::config::PermissionMode;
use crate::config::PortalConfig;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub enum Decision { Allow, Ask, Deny }

pub struct Plan {
    pub decision: Decision,
    pub file: Option<PathBuf>,
    pub host_command: bool,
    pub isolation: &'static str,
    pub reason: &'static str,
}

/// Compute the operation and its scope before asking. Caller-supplied approvals
/// never become capabilities; only ToolHost may apply a successfully reviewed plan.
pub fn plan(config: &PortalConfig, name: &str, args: &Value) -> Result<Plan> {
    let mode = config.security.permission_mode;
    let mut plan = Plan { decision: decide(mode, name), file: None, host_command: false,
        isolation: "host-tool", reason: "External tool or host operation" };
    if mode == PermissionMode::FullAccess { return Ok(plan); }
    if matches!(name, "portal_file_read" | "portal_file_write" | "portal_file_edit" | "portal_file_list" | "portal_search") {
        let write = matches!(name, "portal_file_write" | "portal_file_edit");
        if write && mode == PermissionMode::ReadOnly { return Ok(plan); }
        let path = args.get("path").and_then(Value::as_str)
            .or_else(|| matches!(name, "portal_file_list" | "portal_search").then_some("."))
            .context("Missing file path")?;
        #[cfg(windows)]
        if let Some(std::path::Component::Prefix(prefix)) = std::path::Path::new(path).components().next() {
            use std::path::Prefix;
            anyhow::ensure!(!matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(..)),
                "Network/device paths require full access; they cannot be inspected before approval");
        }
        let target = super::file::approval_target(config, path, name == "portal_file_write")?;
        if mode == PermissionMode::ReadOnly && !target.starts_with(super::file::canonical_workspace_root(config)?) {
            plan.decision = Decision::Deny;
            return Ok(plan);
        }
        let needs_approval = if write { super::file::authorize_write(config, &target).is_err() }
            else { super::file::sensitive_read(config, &target)? };
        plan.decision = if needs_approval {
            if mode == PermissionMode::ReadOnly { Decision::Deny } else { Decision::Ask }
        } else { Decision::Allow };
        plan.isolation = "file-scope";
        plan.reason = if write { "Write outside the workspace or change protected configuration" }
            else { "Read sensitive local data" };
        plan.file = Some(target);
    } else if name == "portal_exec" && mode != PermissionMode::ReadOnly {
        let command = args.get("command").and_then(Value::as_str).context("Missing command")?;
        let scope = match args.get("sandbox_permissions") {
            None => "use_default",
            Some(Value::String(value)) if matches!(value.as_str(), "use_default" | "require_escalated") => value,
            _ => anyhow::bail!("sandbox_permissions must be use_default or require_escalated"),
        };
        if command.trim_start().starts_with('@') {
            anyhow::ensure!(scope == "use_default", "Client commands do not support shell escalation");
            plan.decision = Decision::Ask;
        } else if scope == "require_escalated" {
            anyhow::ensure!(args.get("justification").and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty() && s.len() <= 2000),
                "Host execution requires a justification");
            anyhow::ensure!(!args.get("background").and_then(super::value_as_bool).unwrap_or(false),
                "An approved host command must run in the foreground; background sessions would outlive this approval");
            plan.decision = Decision::Ask;
            plan.host_command = true;
            plan.reason = "Run this command outside the sandbox with host file and network access";
        } else {
            plan.decision = Decision::Allow;
            plan.isolation = if cfg!(windows) { "windows-lpac-offline" } else if cfg!(target_os = "macos") { "macos-seatbelt-offline" } else { "unavailable" };
            plan.reason = "Run inside the workspace sandbox";
        }
    } else if name == "portal_process" && mode != PermissionMode::ReadOnly {
        // These sessions were created by this ToolHost, under its sandbox policy.
        plan.decision = Decision::Allow;
    } else if name == "portal_web_fetch" {
        plan.isolation = "public-web-read";
        plan.reason = "Fetch a public URL without local credentials";
    }
    Ok(plan)
}

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
#[path = "permission_tests.rs"]
mod permission_tests;

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
        let args = json!({"command":"echo should-not-run", "sandbox_permissions":"require_escalated", "justification":"test host access", "approved":true, "approval":"allow-once"});
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
    async fn sandbox_operations_are_automatic_but_host_and_client_commands_ask() {
        let mut config = PortalConfig::default();
        config.security.permission_mode = PermissionMode::Supervised;
        let approver = Arc::new(Approver { requests: Mutex::new(vec![]), allow: false });
        let host = ToolHost::new(&config).with_client_handler(approver.clone());
        let normal = plan(&config, "portal_exec", &json!({"command":"echo hello"})).unwrap();
        assert_eq!(normal.decision, Decision::Allow);
        assert!(!normal.host_command);
        assert!(host.call_with_scene("portal_exec", json!({"command":"echo hello", "sandbox_permissions":"require_escalated", "justification":"host access"}), Some("test-scene")).await.is_err());
        assert!(host.call_with_scene("portal_exec", json!({"command":"  @plugins list"}), Some("test-scene")).await.is_err());
        let requests = approver.requests.lock().unwrap();
        let shell: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
        let client: serde_json::Value = serde_json::from_str(&requests[1]).unwrap();
        assert_eq!(shell["isolation"], "host-tool");
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
