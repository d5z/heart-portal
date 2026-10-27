use super::*;
use crate::{config::PermissionMode, exec_policy::ExecShell};
use std::{fs, os::unix::fs::symlink, path::PathBuf, process::{Output, Stdio}};

struct Fixture { base: PathBuf, root: PathBuf, config: PortalConfig }
impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("portal-seatbelt-{}", uuid::Uuid::new_v4()));
        // Exercise spaces, quotes, Unicode and policy-looking characters without interpolation.
        let root = base.join("工作区 space \" (deny default)");
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let mut config = PortalConfig::default();
        config.security.permission_mode = PermissionMode::Supervised;
        config.security.workspace_root = root.clone();
        Self { base, root, config }
    }
    async fn run(&self, source: &str) -> Output {
        crate::sandbox::shell_command(&self.config, source, self.root.to_str().unwrap(), ExecShell::Default)
            .unwrap().stdin(Stdio::null()).output().await.unwrap()
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.base); } }
fn quote(path: &Path) -> String { format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''")) }
fn success(output: &Output) { assert!(output.status.success(), "{output:?}"); }

#[tokio::test]
async fn workspace_shell_and_children_work_with_a_clean_environment() {
    let f = Fixture::new();
    let out = f.run("mkdir nested; /bin/sh -c 'echo approved > nested/result'; cat nested/result; /usr/bin/env").await;
    success(&out);
    assert_eq!(fs::read_to_string(f.root.join("nested/result")).unwrap(), "approved\n");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("approved\n"));
    for line in text.lines().skip(1) {
        let key = line.split('=').next().unwrap();
        assert!(["PATH", "HOME", "TMPDIR", "LANG", "PWD", "SHLVL", "_"].contains(&key), "Unexpected inherited variable: {key}");
    }
}

#[tokio::test]
async fn outside_files_symlinks_and_network_are_denied() {
    let f = Fixture::new();
    let outside = f.base.join("secret");
    fs::write(&outside, "outside secret").unwrap();
    symlink(&outside, f.root.join("alias")).unwrap();
    for command in [format!("cat {}", quote(&outside)), "cat alias".into(), "/bin/sh -c 'cat alias'".into(),
        format!("echo changed > {}", quote(&outside)), "echo changed > alias".into(),
        format!("ln {} new-link", quote(&outside)),
        // A listening loopback port distinguishes denial from connection refusal.
    ] {
        assert!(!f.run(&command).await.status.success(), "Allowed: {command}");
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let out = f.run(&format!("/usr/bin/curl --noproxy '*' --connect-timeout 1 --max-time 2 http://127.0.0.1:{}/", listener.local_addr().unwrap().port())).await;
    assert!(!out.status.success(), "{out:?}");
    listener.set_nonblocking(true).unwrap();
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside secret");
}

#[tokio::test]
async fn protected_paths_cannot_be_modified_or_renamed_and_secrets_cannot_be_read() {
    let f = Fixture::new();
    fs::create_dir(f.root.join(".git")).unwrap();
    fs::write(f.root.join(".git/config"), "protected").unwrap();
    fs::write(f.root.join(".ENV.local"), "secret").unwrap();
    fs::write(f.root.join("portal.toml"), "protected").unwrap();
    symlink(f.root.join(".ENV.local"), f.root.join("secret-alias")).unwrap();
    for source in ["echo x > .git/config", "mv .git/config config-copy", "mv .git moved-git", "echo x > portal.toml",
        "cat .ENV.local", "cat secret-alias", "mv .ENV.local renamed", "ln .git/config linked", "echo x > .codex"] {
        assert!(!f.run(source).await.status.success(), "Allowed: {source}");
    }
    success(&f.run("cat .git/config").await);
    assert_eq!(fs::read_to_string(f.root.join(".git/config")).unwrap(), "protected");
}

#[tokio::test]
async fn active_custom_config_and_ancestors_cannot_be_moved() {
    let mut f = Fixture::new();
    fs::create_dir(f.root.join("settings")).unwrap();
    let config_path = f.root.join("settings/custom.conf");
    fs::write(&config_path, "token").unwrap();
    f.config.config_path = Some(config_path.clone());
    for source in ["cat settings/custom.conf", "echo x > settings/custom.conf", "mv settings/custom.conf renamed", "mv settings moved"] {
        assert!(!f.run(source).await.status.success(), "Allowed: {source}");
    }
    assert_eq!(fs::read_to_string(config_path).unwrap(), "token");
}

#[tokio::test]
async fn preexisting_hardlinks_fail_closed_without_running_source() {
    let f = Fixture::new();
    let outside = f.base.join("secret");
    fs::write(&outside, "untouched").unwrap();
    fs::hard_link(&outside, f.root.join("alias")).unwrap();
    assert!(shell_command(&f.config, "echo x > marker", f.root.to_str().unwrap()).unwrap_err().to_string().contains("hardlinked"));
    assert!(!f.root.join("marker").exists());
}

#[tokio::test]
async fn modes_and_outside_workdirs_have_no_implicit_fallback() {
    let mut f = Fixture::new();
    assert!(shell_command(&f.config, "echo x", f.base.to_str().unwrap()).is_err());
    f.config.security.permission_mode = PermissionMode::ReadOnly;
    assert!(crate::sandbox::shell_command(&f.config, "echo x", f.root.to_str().unwrap(), ExecShell::Default).is_err());
    f.config.security.permission_mode = PermissionMode::FullAccess;
    let outside = f.base.join("full-access");
    success(&f.run(&format!("echo full > {}", quote(&outside))).await);
    assert_eq!(fs::read_to_string(outside).unwrap(), "full\n");
}

#[tokio::test]
async fn background_commands_inherit_policy_and_cancellation_stops_descendants() {
    use crate::{heart_callback::HeartCallback, process_manager::{ProcessManager, ProcessStatus}};
    use std::time::Duration;
    let f = Fixture::new();
    fs::write(f.base.join("secret"), "outside secret").unwrap();
    let manager = ProcessManager::new(HeartCallback::new());
    let blocked = manager.spawn(&f.config, "/bin/sh -c 'cat ../secret'", f.root.to_str().unwrap(), &[]).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = manager.poll(&blocked.session_id, 0, 100).await.unwrap();
            if let ProcessStatus::Exited(code) = result.status { assert_ne!(code, 0); break result; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    assert!(!String::from_utf8_lossy(&result.output).contains("outside secret"));
    let running = manager.spawn(&f.config, "(sleep 2; echo leaked > escaped) & echo ready; wait", f.root.to_str().unwrap(), &[]).await.unwrap();
    let output = manager.poll(&running.session_id, 0, 2000).await.unwrap();
    assert!(String::from_utf8_lossy(&output.output).contains("ready"));
    manager.kill(&running.session_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert!(!f.root.join("escaped").exists());
}

#[tokio::test]
async fn a_checkout_under_a_reserved_parent_is_not_mistaken_for_protected_config() {
    let mut f = Fixture::new();
    f.root = f.root.join(".codex/worktrees/checkout");
    fs::create_dir_all(&f.root).unwrap();
    f.config.security.workspace_root = f.root.clone();
    success(&f.run("echo ok > normal; cat normal").await);
    assert!(!f.run("mkdir .git; echo blocked > .git/config").await.status.success());
}
