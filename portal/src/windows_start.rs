//! A standalone Windows exe bootstraps the same supervisor used by upgrades.
//! No installed scripts, Rust, Python, VBScript, or manual setup is required.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

pub async fn run(
    action: &str,
    config: Option<&str>,
    connect: Option<&str>,
    name: Option<&str>,
    logs: bool,
) -> Result<()> {
    if action == "start" {
        println!("Heart Portal {}", crate::upgrade::PORTAL_VERSION);
        println!("[启动] 正在检查配置和运行状态……");
        let _ = std::io::stdout().flush();
    }
    if action == "start" && crate::windows_upgrade::recover_interrupted()? {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let root = crate::windows_upgrade::installation_root(&exe)?;
    crate::windows_private::protect_installation(&root)?;
    // Delegation and callers can supply the link without putting its token in
    // argv. This is still an explicit override of the saved launch settings.
    let connect_override = connect.map(str::to_owned).or_else(|| {
        std::env::var("PORTAL_CONNECT_LINK").ok().filter(|value| !value.trim().is_empty())
    });
    let connect = connect_override.as_deref();
    let explicit = config.is_some() || connect.is_some() || name.is_some();
    let saved = root.join(".portal-launch.json");
    let (launch, initialize_config) = if action != "start" || (!explicit && saved.is_file()) {
        (Value::Null, false)
    } else {
        make_launch(&root, config, connect, name)?
    };
    if action == "start" {
        let effective = if launch.is_null() {
            serde_json::from_slice(&crate::bounded_file::metadata_bytes(&saved)?)
                .context("Invalid saved Portal launch configuration")?
        } else {
            launch.clone()
        };
        print_launch_summary(&effective, &exe);
    }
    let stage = root
        .join(".portal-start")
        .join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&stage).context(
        "Portal needs a writable folder; move the exe to a folder owned by your Windows user",
    )?;
    crate::windows_upgrade::export_runtime(&stage.join("support"))?;
    std::fs::write(
        stage.join("portal-lifecycle.ps1"),
        include_str!("../../scripts/portal-lifecycle.ps1"),
    )?;
    std::fs::write(
        stage.join("portal-task-common.ps1"),
        include_str!("../../scripts/portal-task-common.ps1"),
    )?;
    let worker = stage.join("portal-start-worker.ps1");
    std::fs::write(
        &worker,
        include_str!("../../scripts/portal-start-worker.ps1"),
    )?;
    let request = json!({
        "action": action, "root": root, "target": exe, "launch": launch,
        "initialize_config": initialize_config, "explicit": explicit,
        "parent_pid": std::process::id(), "version": crate::upgrade::PORTAL_VERSION,
    });
    crate::windows_upgrade::write_json(&stage.join("request.json"), &request)?;
    let powershell =
        PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is missing")?)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = tokio::process::Command::new(powershell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(worker)
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000 | 0x0000_0200)
        .kill_on_drop(true);
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        stream_worker(&mut command, action),
    )
    .await;
    // These helper files are disposable; running supervisors use root/scripts.
    // Remove only the fixed files we created, never recurse through user data.
    for name in [
        "portal-lifecycle.ps1",
        "portal-supervisor.ps1",
        "portal-supervisor-bootstrap.ps1",
        "portal-supervisor-hidden.vbs",
        "portal-background-v1.exe",
    ] {
        let _ = std::fs::remove_file(stage.join("support").join(name));
    }
    let _ = std::fs::remove_dir(stage.join("support"));
    for name in [
        "portal-lifecycle.ps1",
        "portal-task-common.ps1",
        "portal-start-worker.ps1",
        "request.json",
    ] {
        let _ = std::fs::remove_file(stage.join(name));
    }
    let _ = std::fs::remove_dir(&stage);
    let output = result.context("Timed out starting Portal supervision; inspect portal-runtime.err.log and .portal-start-status.json")?
        .context("Windows PowerShell could not start the Portal supervisor")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        bail!("Portal {action} failed: {}", detail.trim());
    }
    if action == "start" {
        println!("[就绪] Portal 和守护已在后台运行。");
        // Repeat the actionable summary after progress, so it isn't buried.
        if let Ok(bytes) = crate::bounded_file::metadata_bytes(&saved) {
            if let Ok(effective) = serde_json::from_slice::<Value>(&bytes) {
                print_launch_summary(&effective, &exe);
            }
        }
        println!("[日志] {}", root.join("portal-runtime.log").display());
        println!("[日志] {}", root.join("portal-runtime.err.log").display());
        // The EXE must exit after handoff so an upgrade can replace it. A
        // separate PowerShell reader keeps an interactive console useful.
        // Redirected/script callers still return promptly and close their pipes.
        if logs && std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            println!("[日志] 正在接入实时日志；关闭窗口或按 Ctrl+C 只退出日志查看，Portal 和守护继续运行。");
            println!(
                "[停止] 要停止 Portal，请另开 PowerShell 窗口执行：{} stop",
                powershell_exe(&exe)
            );
            let _ = std::io::stdout().flush();
            if let Err(error) = start_console(&root) {
                eprintln!("[日志] 无法打开实时日志：{error}。可直接查看上面的日志文件。");
            }
        }
    }
    Ok(())
}

async fn stream_worker(
    command: &mut tokio::process::Command,
    action: &str,
) -> Result<std::process::Output> {
    let mut child = command.spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().context("Missing worker stdout")?).lines();
    let mut stderr = BufReader::new(child.stderr.take().context("Missing worker stderr")?).lines();
    let (mut out_done, mut err_done) = (false, false);
    let mut errors = Vec::new();
    let started = std::time::Instant::now();
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );
    while !out_done || !err_done {
        tokio::select! {
            line = stdout.next_line(), if !out_done => match line? {
                Some(line) => {
                    println!("{}", progress_text(&line));
                    let _ = std::io::stdout().flush();
                }
                None => out_done = true,
            },
            line = stderr.next_line(), if !err_done => match line? {
                Some(line) => { errors.extend_from_slice(line.as_bytes()); errors.push(b'\n'); }
                None => err_done = true,
            },
            _ = heartbeat.tick(), if action == "start" => {
                println!("[启动] 仍在等待启动完成（{} 秒）；配置加载、工具初始化及就绪检查可能需要一些时间。", started.elapsed().as_secs());
                let _ = std::io::stdout().flush();
            }
        }
    }
    Ok(std::process::Output {
        status: child.wait().await?,
        stdout: Vec::new(),
        stderr: errors,
    })
}

fn progress_text(line: &str) -> &str {
    match line {
        "PORTAL_PROGRESS:lock" => "[启动] 正在协调已有实例、守护和升级状态……",
        "PORTAL_PROGRESS:reuse" => "[启动] 检测到已有守护，正在确认 Portal 状态……",
        "PORTAL_PROGRESS:config" => "[配置] 正在准备配置文件和守护脚本……",
        "PORTAL_PROGRESS:task" => "[守护] 正在配置登录自启并启动后台守护……",
        "PORTAL_PROGRESS:ready" => "[启动] 守护已接手，正在等待 Portal 初始化并稳定运行……",
        _ => line,
    }
}

fn powershell_exe(exe: &Path) -> String {
    format!("& {}", powershell_quote(&exe.display().to_string()))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn connect_command(launch: &Value, exe: &Path) -> String {
    format!(
        "{} --config {} --name {} --connect $beingLink",
        powershell_exe(exe),
        powershell_quote(launch["arguments"][1].as_str().unwrap_or("portal.toml")),
        powershell_quote(launch["name"].as_str().unwrap_or("portal")),
    )
}

fn print_launch_summary(launch: &Value, exe: &Path) {
    if let Some(config) = launch["arguments"][1].as_str() {
        println!("[配置] {config}");
    }
    let connection = launch["environment"]["PORTAL_CONNECT_LINK"]
        .as_str()
        .unwrap_or("")
        .trim();
    if connection.is_empty() {
        println!("\n============================================================");
        println!("  【需要配置】尚未配置 Being 连接，当前仅提供本地 MCP 服务");
        println!("============================================================");
        println!("请在 PowerShell 中依次执行下面三行：");
        println!("$beingLink = Read-Host '请粘贴从 Beings 复制的完整连接链接（包含 token=）'");
        let command = powershell_exe(exe);
        println!("{command} stop");
        println!("{}", connect_command(launch, exe));
        println!("--connect 接收完整链接，不能只填写 token；以上命令保留当前配置和 Portal 名称。");
        println!("============================================================\n");
    } else {
        // Never echo the saved URL or token. Configured is not connected.
        println!("\n========== Being 连接：已配置链接 ==========");
        println!("已配置 Being 链接；后台将尝试连接。可在日志文件中查看连接状态，或在终端使用 --logs 查看实时日志。");
        println!("==========================================\n");
    }
    let _ = std::io::stdout().flush();
}

fn start_console(root: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    let powershell =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    std::process::Command::new(powershell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            include_str!("../../scripts/portal-console.ps1"),
        ])
        .env("HEART_PORTAL_CONSOLE_ROOT", root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .creation_flags(0)
        .spawn()?;
    Ok(())
}

fn make_launch(
    root: &Path,
    config: Option<&str>,
    connect: Option<&str>,
    name: Option<&str>,
) -> Result<(Value, bool)> {
    // Changing only --name/--connect must retain the saved explicit config.
    let config_path =
        crate::paths::locate_config(config.map(Path::new), &[root.to_path_buf()])?.path;
    let (resolved, initialize_config) = if config_path.try_exists()? {
        (
            crate::config::PortalConfig::load(
                config_path
                    .to_str()
                    .context("Config path must be Unicode")?,
            )?,
            false,
        )
    } else {
        anyhow::ensure!(
            config.is_none(),
            "Config file not found: {}",
            config_path.display()
        );
        let mut defaults = crate::config::PortalConfig::default();
        defaults.bind_host = "127.0.0.1".into();
        (defaults, true)
    };
    let connection = connect
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("PORTAL_CONNECT_LINK")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| resolved.connect_link.clone());
    let connection = match connection {
        Some(value) => Some(value),
        None => crate::bounded_file::optional_text(&root.join(".portal-connection.url"))?
            .map(|value| value.trim().to_owned()).filter(|value| !value.is_empty()),
    };
    let identity = if let Some(link) = &connection {
        let (host, being, _) = crate::relay_client::parse_loom_link(link)?;
        format!("{}/{being}", host.to_ascii_lowercase())
    } else {
        format!("standalone/{}:{}", resolved.bind_host, resolved.bind_port)
    };
    let saved_name = crate::bounded_file::optional_text(&root.join(".portal-name"))?;
    let portal_name = crate::relay_portal_name(
        name.map(str::to_owned)
            .or_else(|| saved_name.map(|value| value.trim().to_owned())),
        &resolved.name,
        crate::default_relay_portal_name,
    );
    let mut environment: BTreeMap<String, String> = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "PORTAL_MCP_TOKEN",
        "RUST_LOG",
    ]
    .iter()
    .filter_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| (key.to_string(), value))
    })
    .collect();
    // The relay credential is passed through the child's environment, not argv.
    environment.insert("PORTAL_CONNECT_LINK".into(), connection.unwrap_or_default());
    Ok((
        json!({
            "protocol": 1, "identity": identity, "name": portal_name,
            "arguments": ["--config", config_path, "--name", portal_name],
            "working_directory": std::env::current_dir()?, "environment": environment,
        }),
        initialize_config,
    ))
}
