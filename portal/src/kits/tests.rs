use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use super::{environment::KitEnvironment, loader, manager::KitManager, manifest::KitManifest};

pub(crate) struct TestKits(pub PathBuf);

impl TestKits {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("portal-kit-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub fn install(&self, name: &str, env: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "name": name, "version": "1.0.0",
            "command": [std::env::current_exe().unwrap(), "--exact", "kits::tests::mcp_fixture", "--nocapture", "--quiet"],
            "tools": [{"name": "ping", "description": "Fixture", "params": {"type": "object"}}],
            "provision": {"env": [{"name": "PORTAL_TEST_KIT_TOKEN", "required": true, "description": "Test credential"}]}
        });
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        self.write_env(&dir, env);
        dir
    }

    pub fn write_env(&self, dir: &Path, env: &str) {
        std::fs::write(
            dir.join(".env"),
            format!("PORTAL_TEST_KIT_FIXTURE=1\n{env}\n"),
        )
        .unwrap();
    }

    pub fn scan(&self) -> loader::KitScan {
        loader::scan_kits_from_dir(&self.0).unwrap()
    }
}

impl Drop for TestKits {
    fn drop(&mut self) {
        // This is the exact unique directory this fixture created, never a kit
        // path supplied by an external manifest or a user's installed directory.
        assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scanner_accepts_utf8_bom_and_ignores_retired_backup_directory() {
    let root = TestKits::new();
    let active = root.install("active", "PORTAL_TEST_KIT_TOKEN=test");
    let manifest = std::fs::read(active.join("manifest.json")).unwrap();
    let mut bom_manifest = vec![0xEF, 0xBB, 0xBF];
    bom_manifest.extend(manifest);
    std::fs::write(active.join("manifest.json"), bom_manifest).unwrap();

    let retired = root.0.join("kit-retired");
    std::fs::create_dir_all(&retired).unwrap();
    std::fs::write(retired.join("manifest.json"), b"{").unwrap();

    let scan = root.scan();
    assert_eq!(scan.kits.len(), 1);
    assert_eq!(scan.kits[0].manifest.name, "active");
    assert!(scan.invalid_dirs.is_empty());
}

fn edit_manifest(dir: &Path, edit: impl FnOnce(&mut Value)) {
    let path = dir.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);
    std::fs::write(path, manifest.to_string()).unwrap();
}

#[tokio::test]
async fn invalid_manifest_retains_running_owner_against_another_directory() {
    let root = TestKits::new();
    let old = root.install("sample", "PORTAL_TEST_KIT_TOKEN=original");
    let manager = KitManager::new(root.scan().kits);
    let first = manager
        .call_tool("sample", "ping", json!({}))
        .await
        .unwrap();
    std::fs::write(old.join("manifest.json"), "{").unwrap();
    let replacement = root.install("replacement", "PORTAL_TEST_KIT_TOKEN=replacement");
    edit_manifest(&replacement, |m| m["name"] = json!("sample"));
    for target in [None, Some("sample"), None] {
        let report = manager.refresh_kits_target(root.scan(), true, target).await;
        assert!(!report.changed());
        assert_eq!(report.retained_invalid, ["sample"]);
        let current = manager
            .call_tool("sample", "ping", json!({}))
            .await
            .unwrap();
        assert_eq!(current["pid"], first["pid"]);
        assert_eq!(current["token"], "original");
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn retained_and_target_excluded_routes_cannot_be_taken_over() {
    for invalid in [true, false] {
        let root = TestKits::new();
        let owner = root.install("a-b", "PORTAL_TEST_KIT_TOKEN=original");
        let manager = KitManager::new(root.scan().kits);
        let generation = manager.statuses().await[0].diagnostics.generation.clone();
        if invalid {
            std::fs::write(owner.join("manifest.json"), "{").unwrap();
        } else {
            edit_manifest(&owner, |m| m["tools"][0]["name"] = json!("pong"));
        }
        let other = root.install("a", "PORTAL_TEST_KIT_TOKEN=other");
        edit_manifest(&other, |m| m["tools"][0]["name"] = json!("b_ping"));
        let report = manager
            .refresh_kits_target(root.scan(), true, Some("a"))
            .await;
        assert!(!report.changed());
        let statuses = manager.statuses().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].diagnostics.generation, generation);
        assert_eq!(manager.resolve_tool("a_b_ping").await.unwrap().0, "a-b");
        manager.shutdown().await;
    }
}

#[test]
fn normalized_tool_aliases_are_rejected_during_discovery() {
    let root = TestKits::new();
    let dir = root.install("sample", "PORTAL_TEST_KIT_TOKEN=test");
    edit_manifest(&dir, |m| {
        let mut alias = m["tools"][0].clone();
        alias["name"] = json!("get_item");
        m["tools"][0]["name"] = json!("get-item");
        m["tools"].as_array_mut().unwrap().push(alias);
    });
    let scan = root.scan();
    assert!(scan.kits.is_empty());
    assert_eq!(scan.invalid_dirs, [dir]);
}

#[tokio::test]
async fn kit_path_resolves_runtime_and_refreshes_after_installation() {
    let root = TestKits::new();
    let dir = root.install("sample", "PORTAL_TEST_KIT_TOKEN=test");
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let command = "portal-kit-runtime-fixture";
    #[cfg(windows)]
    let executable = bin.join(format!("{command}.exe"));
    #[cfg(not(windows))]
    let executable = bin.join(command);
    // A relative PATH must use the kit cwd, not the Portal launch directory.
    root.write_env(&dir, "PORTAL_TEST_KIT_TOKEN=test\nPATH=bin\nPATHEXT=.EXE");
    edit_manifest(&dir, |m| m["command"][0] = json!(command));
    let manager = KitManager::new(root.scan().kits);
    assert_eq!(manager.statuses().await[0].status, "unhealthy");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    assert!(manager.refresh_kits(root.scan(), false).await);
    assert_eq!(manager.statuses().await[0].status, "not-started");
    let result = manager
        .call_tool("sample", "ping", json!({}))
        .await
        .unwrap();
    assert_eq!(result["token"], "test");
    let pid = result["pid"].clone();
    assert!(!manager.refresh_kits(root.scan(), false).await);
    assert_eq!(
        manager
            .call_tool("sample", "ping", json!({}))
            .await
            .unwrap()["pid"],
        pid
    );
    manager.shutdown().await;
}

// Spawn this test executable itself as an MCP fixture: no Node/Python dependency
// and the same process/environment checks run on Windows, Linux and macOS.
#[test]
fn mcp_fixture() {
    if std::env::var("PORTAL_TEST_KIT_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    use std::io::{BufRead, Write};
    let code = std::fs::read_to_string("code.txt").unwrap_or_default();
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        let response = if request["method"] == "tools/call" {
            if request["params"]["arguments"]["wait_error"] == true {
                std::fs::write("started", "1").unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                while !Path::new("release").exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "old call failed"}})
            } else {
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "token": std::env::var("PORTAL_TEST_KIT_TOKEN").unwrap_or_default(),
                    "kit": std::env::var("PORTAL_KIT_NAME").unwrap_or_default(),
                    "pid": std::process::id(), "code": code,
                }})
            }
        } else {
            json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}})
        };
        println!("{response}");
        std::io::stdout().flush().unwrap();
    }
    std::process::exit(0);
}

#[test]
fn dotenv_parsing_precedence_and_redaction() {
    let root = TestKits::new();
    let dir = root.install("jira", "# comment\r\nexport PORTAL_TEST_KIT_TOKEN='a=b # $literal'\r\nMULTILINE=\"one\\ntwo\"\r\nEMPTY=\r\n");
    let mut manifest: KitManifest =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    manifest.provision.as_mut().unwrap().env.extend([
        super::manifest::KitEnvVar {
            name: "PATH".into(),
            description: None,
            required: true,
            default: Some("fallback".into()),
        },
        super::manifest::KitEnvVar {
            name: "PORTAL_TEST_DEFAULT".into(),
            description: None,
            required: false,
            default: Some("default-value".into()),
        },
    ]);
    let env = KitEnvironment::load(&dir, &manifest);
    assert!(env.error.is_none(), "{:?}", env.error);
    assert_eq!(env.values["PORTAL_TEST_KIT_TOKEN"], "a=b # $literal");
    assert_eq!(env.values["MULTILINE"], "one\ntwo");
    assert_eq!(env.values["EMPTY"], "");
    assert_eq!(env.values["PATH"], "fallback");
    assert_eq!(env.values["PORTAL_TEST_DEFAULT"], "default-value");
    assert!(std::env::var("PORTAL_TEST_KIT_TOKEN").is_err());
    let status = serde_json::to_string(&env.statuses(&manifest)).unwrap();
    assert!(!status.contains("a=b") && !format!("{env:?}").contains("a=b"));

    for bad in [
        "",
        "PORTAL_TEST_KIT_TOKEN=",
        "PORTAL_TEST_KIT_TOKEN={{YOUR_TOKEN}}",
        "PORTAL_TEST_KIT_TOKEN='unterminated-secret",
    ] {
        root.write_env(&dir, bad);
        let env = KitEnvironment::load(&dir, &manifest);
        assert!(env.error.is_some());
        assert!(!env.error.unwrap().contains("unterminated-secret"));
    }
    // Legacy kits without provision still receive their kit-local .env.
    manifest.provision = None;
    root.write_env(&dir, "LEGACY_TOKEN='legacy-value'");
    assert_eq!(
        KitEnvironment::load(&dir, &manifest).values["LEGACY_TOKEN"],
        "legacy-value"
    );
    std::fs::write(dir.join(".env"), "\u{feff}LEGACY_TOKEN=bom-value\r\n").unwrap();
    assert_eq!(
        KitEnvironment::load(&dir, &manifest).values["LEGACY_TOKEN"],
        "bom-value"
    );
}

#[tokio::test]
async fn credentials_and_code_reload_without_portal_restart() {
    let root = TestKits::new();
    let dir = root.install("jira", "");
    let manager = KitManager::new(root.scan().kits);
    assert_eq!(manager.statuses().await[0].status, "needs-configuration");
    assert!(manager.list_healthy_tools().await.is_empty());
    for _ in 0..4 {
        let error = manager
            .call_tool("jira", "ping", json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("PORTAL_TEST_KIT_TOKEN"));
    }
    root.write_env(
        &dir,
        "PORTAL_TEST_KIT_TOKEN='first-secret'\nPORTAL_KIT_NAME=forged",
    );
    assert!(manager.refresh_kits(root.scan(), false).await);
    let first = manager.call_tool("jira", "ping", json!({})).await.unwrap();
    assert_eq!(first["token"], "first-secret");
    assert_eq!(first["kit"], "jira");
    assert!(!manager.refresh_kits(root.scan(), false).await);
    assert_eq!(
        manager.call_tool("jira", "ping", json!({})).await.unwrap()["pid"],
        first["pid"]
    );

    root.write_env(&dir, "PORTAL_TEST_KIT_TOKEN='second-secret'");
    assert!(manager.refresh_kits(root.scan(), false).await);
    let second = manager.call_tool("jira", "ping", json!({})).await.unwrap();
    assert_eq!(second["token"], "second-secret");
    assert_ne!(second["pid"], first["pid"]);
    std::fs::write(dir.join("code.txt"), "updated-code").unwrap();
    assert!(manager.refresh_kits(root.scan(), true).await);
    assert_eq!(
        manager.call_tool("jira", "ping", json!({})).await.unwrap()["code"],
        "updated-code"
    );
    let status = serde_json::to_string(&manager.statuses().await).unwrap();
    assert!(!status.contains("second-secret"));
    assert_eq!(manager.drain_usage_counts().await["jira"], 4);
    manager.shutdown().await;
}

#[tokio::test]
async fn discovery_preserves_partial_manifest_and_removes_uninstalled_kit() {
    let root = TestKits::new();
    let manager = KitManager::new(vec![]);
    let dir = root.install("new-kit", "PORTAL_TEST_KIT_TOKEN=ok");
    assert!(manager.refresh_kits(root.scan(), false).await);
    assert!(manager.resolve_tool("new_kit_ping").await.is_some());
    std::fs::write(dir.join("manifest.json"), "{").unwrap();
    assert!(!manager.refresh_kits(root.scan(), false).await);
    assert!(manager.resolve_tool("new_kit_ping").await.is_some());
    std::fs::remove_file(dir.join("manifest.json")).unwrap();
    assert!(!manager.refresh_kits(root.scan(), false).await);
    // Moving outside the scanned directory is an uninstall without destructive cleanup.
    let removed = root.0.join("removed");
    std::fs::create_dir(&removed).unwrap();
    std::fs::rename(dir, removed.join("new-kit")).unwrap();
    assert!(manager.refresh_kits(root.scan(), false).await);
    assert!(manager.list_healthy_tools().await.is_empty());
    assert!(manager.resolve_tool("new_kit_ping").await.is_none());
}

#[tokio::test]
async fn in_flight_failure_does_not_close_replacement_connection() {
    let root = TestKits::new();
    let dir = root.install("jira", "PORTAL_TEST_KIT_TOKEN=old");
    let manager = KitManager::new(root.scan().kits);
    let old_manager = manager.clone();
    let old_call = tokio::spawn(async move {
        old_manager
            .call_tool("jira", "ping", json!({"wait_error": true}))
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !dir.join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    root.write_env(&dir, "PORTAL_TEST_KIT_TOKEN=new");
    assert!(manager.refresh_kits(root.scan(), false).await);
    let replacement = manager.call_tool("jira", "ping", json!({})).await.unwrap();
    assert_eq!(replacement["token"], "new");
    std::fs::write(dir.join("release"), "1").unwrap();
    assert!(old_call.await.unwrap().is_err());
    let after = manager.call_tool("jira", "ping", json!({})).await.unwrap();
    assert_eq!(after["pid"], replacement["pid"]);
    assert_eq!(manager.statuses().await[0].status, "healthy");
    manager.shutdown().await;
}
