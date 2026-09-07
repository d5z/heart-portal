//! Native Windows regression tests for the shared foreground/background shell path.
use super::*;
use crate::process_manager::ProcessStatus;
use std::path::PathBuf;
use std::time::Duration;

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("portal exec spaces {}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn config(&self) -> PortalConfig {
        let mut config = PortalConfig::default();
        config.security.workspace_root = self.0.clone();
        config
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn execute(config: &PortalConfig, command: &str, background: bool) -> (bool, String) {
    execute_with_shell(config, command, background, "default").await
}

async fn execute_with_shell(
    config: &PortalConfig,
    command: &str,
    background: bool,
    shell: &str,
) -> (bool, String) {
    let mut config = config.clone();
    config.kits_enabled = false;
    let host = crate::tools::ToolHost::new(&config);
    let manager = host.process_manager.clone();
    let result = host
        .call(
            "portal_exec",
            serde_json::json!({
                "command": command, "background": background, "timeout_secs": 15, "shell": shell,
            }),
        )
        .await
        .unwrap();
    if !background {
        return (
            result["isError"].as_bool().unwrap(),
            result["content"][0]["text"].as_str().unwrap().to_owned(),
        );
    }
    let info: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    let session = info["session_id"].as_str().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let result = manager.poll(session, 0, 100).await.unwrap();
            if let ProcessStatus::Exited(code) = result.status {
                // Exit and pipe-reader completion are independently scheduled.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let output = manager.log(session, 0, 100_000).await.unwrap();
                return (
                    code != 0,
                    String::from_utf8_lossy(&output.output).into_owned(),
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    manager.kill_all().await;
    outcome.expect("background command must exit")
}

#[tokio::test]
#[cfg_attr(not(windows), ignore = "requires native Windows")]
async fn powershell_unicode_script_output_and_utf8_files_work_in_both_modes() {
    let workspace = Workspace::new();
    std::fs::write(workspace.0.join("中文.txt"), "文件中文🙂").unwrap();
    for background in [false, true] {
        let command = "Write-Output '输出中文🙂'; Get-Content -LiteralPath '中文.txt'; '写入中文' | Set-Content -LiteralPath '写入.txt'";
        let (failed, output) =
            execute_with_shell(&workspace.config(), command, background, "powershell").await;
        assert!(
            !failed && output.contains("输出中文🙂") && output.contains("文件中文🙂"),
            "{output}"
        );
        let written = std::fs::read_to_string(workspace.0.join("写入.txt")).unwrap();
        assert!(written.contains("写入中文"));
        let (failed, _) =
            execute_with_shell(&workspace.config(), "exit 7", background, "powershell").await;
        assert!(failed);
    }
}

#[tokio::test]
#[cfg_attr(not(windows), ignore = "requires native Windows")]
async fn quoted_executable_arguments_and_cmd_operators_work_in_both_modes() {
    let workspace = Workspace::new();
    let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
    let exe = workspace.0.join("find text.exe");
    std::fs::copy(system.join("System32/findstr.exe"), &exe).unwrap();
    std::fs::write(workspace.0.join("input file.txt"), "two words\r\nother\r\n").unwrap();
    let quoted = format!(r#""{}" /L /C:"two words" "input file.txt""#, exe.display());
    let config = workspace.config();
    for background in [false, true] {
        for (command, expected) in [
            (quoted.clone(), "two words"),
            (format!("{} && echo chain-ok", quoted), "chain-ok"),
            (
                format!(
                    r#"{} > "output file.txt" && type "output file.txt""#,
                    quoted
                ),
                "two words",
            ),
            (
                r#"echo pipe-ok | findstr /L /C:"pipe-ok""#.into(),
                "pipe-ok",
            ),
            ("echo first & echo second".into(), "second"),
            (
                r#"set "PORTAL_QUOTE_TEST=two words" && echo %COMSPEC%"#.into(),
                "cmd.exe",
            ),
        ] {
            let (failed, output) = execute(&config, &command, background).await;
            assert!(
                !failed && output.to_lowercase().contains(expected),
                "background={background}, command={command}, output={output}"
            );
        }
        let (failed, output) = execute(
            &config,
            &format!(r#""{}" /L /C:"absent" "input file.txt""#, exe.display()),
            background,
        )
        .await;
        assert!(failed, "nonzero exit must be preserved: {output}");
    }
}

#[tokio::test]
#[cfg_attr(not(windows), ignore = "requires native Windows")]
async fn installed_git_tools_are_available_to_exec_children() {
    let git_tools = std::env::split_paths(&windows_exec_path())
        .find(|path| path.join("ls.exe").is_file() && path.join("bash.exe").is_file());
    if git_tools.is_none() {
        // Git is optional on end-user machines; CI installs it explicitly.
        eprintln!("Git for Windows not installed; skipping optional tools test");
        return;
    }
    let workspace = Workspace::new();
    let bash = git_tools.unwrap().join("bash.exe");
    for background in [false, true] {
        let (failed, output) = execute(
            &workspace.config(),
            &format!(r#""{}" -c "echo ok""#, bash.display()),
            background,
        )
        .await;
        assert!(
            !failed && output.trim() == "ok",
            "quoted Git Bash: {output}"
        );
        std::fs::write(workspace.0.join("scratch file.txt"), "two words\n").unwrap();
        let command = r#"ls "scratch file.txt" && cat "scratch file.txt" | grep "two words" && rm "scratch file.txt""#;
        let (failed, output) = execute(&workspace.config(), command, background).await;
        assert!(!failed && output.contains("two words"), "{output}");
        assert!(!workspace.0.join("scratch file.txt").exists());
    }
}
