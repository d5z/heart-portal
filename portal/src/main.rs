//! Heart Portal — Being's gateway to the world.
//!
//! A lightweight MCP server with built-in tools (exec, file, web).
//! Heart's MCP supervisor connects to Portal via TCP.
//! Portal can run on Town Home, a human's laptop, or anywhere.

mod bounded_file;
mod config;
mod connection_status;
mod exec_policy;
mod heart_callback;
mod kits;
#[cfg(target_os = "macos")]
mod macos_supervisor;
#[cfg(target_os = "macos")]
mod macos_upgrade;
mod mcp;
mod paths;
mod process_manager;
mod protocol;
mod relay_client;
mod single_instance;
mod subagent;
mod tools;
mod upgrade;
#[cfg(any(windows, target_os = "macos"))]
mod user_installation;
#[cfg(windows)]
mod windows_private;
#[cfg(windows)]
mod windows_start;
#[cfg(windows)]
mod windows_upgrade;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tracing::{debug, info, trace, warn};

use crate::config::PortalConfig;
use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, PORTAL_VERSION};
use crate::tools::ToolHost;

#[derive(Parser)]
#[command(
    name = "heart-portal",
    version = PORTAL_VERSION,
    about = "Heart Portal — Being's gateway to the world"
)]
struct Cli {
    /// Prepare the stable user installation for OS installers without starting it
    #[arg(long, hide = true)]
    install_user_runtime: bool,
    /// Export version-matched Windows supervision code for the update worker
    #[arg(long, hide = true)]
    export_windows_runtime: Option<PathBuf>,
    /// Legacy spelling for the upgrade subcommand
    #[arg(long = "upgrade", hide = true)]
    legacy_upgrade: bool,
    #[command(subcommand)]
    command: Option<Commands>,

    /// Loom URL for reverse relay (Home mode)
    #[arg(long)]
    connect: Option<String>,

    /// Portal name for relay handshake identity
    #[arg(long)]
    name: Option<String>,

    /// Path to portal.toml
    #[arg(short = 'c', long = "config")]
    config: Option<String>,

    /// Path to portal.toml (positional)
    #[arg(value_name = "CONFIG")]
    config_positional: Option<String>,
    /// Override the config file's exec tool setting for managed desktop runs.
    #[arg(long, hide = true)]
    exec_enabled: Option<bool>,
    /// Override the config file's kits setting for managed desktop runs.
    #[arg(long, hide = true)]
    kits_enabled: Option<bool>,
}

#[derive(Subcommand)]
enum Commands {
    /// Install and configure the subagent from a bounded JSON object on stdin.
    SubagentSetup,
    /// Inspect configuration paths or copy legacy configuration into the user directory
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Stop this Windows/macOS Portal and its supervision
    Stop,
    /// Show the running Windows/macOS Portal and supervisor status
    Status,
    /// Check GitHub releases and upgrade to the latest version
    Upgrade {
        /// Apply a pre-downloaded newer Windows/macOS binary through the same updater
        #[arg(long, conflicts_with = "status")]
        file: Option<PathBuf>,
        /// Show the last Windows/macOS upgrade transaction without downloading
        #[arg(long)]
        status: bool,
        /// Migrate an existing macOS installation using this downloaded new executable
        #[arg(long, conflicts_with_all = ["file", "status"])]
        target: Option<PathBuf>,
    },
    /// Manage installed Portal kits
    Kit {
        #[command(subcommand)]
        command: KitCommands,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Show the effective configuration path and the central user directory (no secrets)
    Path,
    /// Create the default user config if absent; preserve existing configuration
    Init,
    /// Preview a non-destructive configuration migration; --apply publishes the copy
    Migrate {
        #[arg(long)]
        from: PathBuf,
        /// Separate named configuration under ~/.heart-portal/profiles/<name>/
        #[arg(long)]
        profile: Option<String>,
        /// Original installation directory when its launch metadata is elsewhere
        #[arg(long)]
        installation: Option<PathBuf>,
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum KitCommands {
    /// List installed kits
    List,
    /// Show kit pre-flight status
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(any(windows, target_os = "macos"))]
    {
        // Older updaters execute the staged candidate with --version before
        // stopping Portal. Reject relocation here, before clap handles that
        // flag: waiting until startup would depend on the old rollback owner.
        let source = std::env::current_exe()?;
        let staged = source.parent().and_then(std::path::Path::parent)
            .is_some_and(|parent| parent.file_name() == Some(std::ffi::OsStr::new(".portal-upgrades")));
        anyhow::ensure!(!staged || user_installation::is_managed(&source)?,
            "Stop the legacy Portal, then launch the new download directly to migrate into ~/.heart-portal before upgrading");
    }
    let runtime_started = std::time::Instant::now();
    let cli = Cli::parse();
    if std::env::var_os("HEART_PORTAL_EXTERNAL_TOOL").is_some() {
        // Prevent accidental recursive host administration from managed MCP
        // processes. This inherited marker is not a hostile-code security check.
        let read_only = matches!(
            &cli.command,
            Some(Commands::Status | Commands::Kit { .. })
                | Some(Commands::Config {
                    command: ConfigCommands::Path
                })
                | Some(Commands::Upgrade { status: true, .. })
        );
        anyhow::ensure!(read_only && !cli.legacy_upgrade && cli.export_windows_runtime.is_none() && !cli.install_user_runtime,
            "Managed external tools cannot start, stop, upgrade or reconfigure Portal; use the host management channel");
    }
    if let Some(path) = &cli.export_windows_runtime {
        #[cfg(windows)]
        return windows_upgrade::export_runtime(path);
        #[cfg(not(windows))]
        anyhow::bail!("Windows runtime export is available only on Windows");
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let source = std::env::current_exe()?;
        let managed = user_installation::is_managed(&source)?;
        let explicit_config = cli.config.as_deref().or(cli.config_positional.as_deref());
        if cli.install_user_runtime {
            let target = user_installation::prepare(&source, explicit_config)?;
            println!("{}", serde_json::json!({"root": user_installation::legacy_root(&target)?, "executable": target}));
            return Ok(());
        }
        let lifecycle = cli.command.is_none() || cli.legacy_upgrade || matches!(
            &cli.command, Some(Commands::Stop | Commands::Status | Commands::Upgrade { .. }));
        // Stop/status still reach an old installation before its controlled move.
        let legacy_management = matches!(&cli.command, Some(Commands::Stop | Commands::Status))
            && user_installation::has_legacy_state(&source)? && !user_installation::migrated_source(&source)?;
        // Town-Client owns the executable, supervisor and upgrade transaction.
        // An explicit config keeps the user's existing workspace and kits.
        let client_managed = std::env::var("HEART_PORTAL_CLIENT_MANAGED").as_deref() == Ok("1")
            && std::env::var("HEART_PORTAL_SUPERVISED").as_deref() == Ok("1")
            && explicit_config.is_some();
        if !managed && lifecycle && !legacy_management && !client_managed {
            anyhow::ensure!(std::env::var("HEART_PORTAL_SUPERVISED").as_deref() != Ok("1"),
                "Stop the legacy supervisor, then launch Portal directly to migrate into ~/.heart-portal");
            if cli.command.is_none() {
                let target = user_installation::prepare(&source, explicit_config)?;
                return user_installation::delegate(&target, &cli);
            }
            let target = user_installation::executable()?;
            anyhow::ensure!(target.is_file(), "No user Portal installation found; start Portal once first");
            return user_installation::delegate(&target, &cli);
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    if cli.install_user_runtime {
        anyhow::bail!("User runtime installation is supported on Windows and macOS");
    }
    let command = cli.command;
    if let Some(Commands::Config { command }) = &command {
        let data = paths::data_dir()?;
        return match command {
            ConfigCommands::Path | ConfigCommands::Init => {
                let explicit = cli
                    .config
                    .as_deref()
                    .or(cli.config_positional.as_deref())
                    .map(std::path::Path::new);
                let location = paths::locate_config(explicit, &paths::legacy_dirs()?)?;
                if matches!(command, ConfigCommands::Init) {
                    paths::initialize_config(&location)?;
                    PortalConfig::load(
                        location
                            .path
                            .to_str()
                            .context("Config path must be Unicode")?,
                    )?;
                }
                println!(
                    "{}",
                    serde_json::json!({"config": location, "user_directory": data})
                );
                Ok(())
            }
            ConfigCommands::Migrate {
                from,
                profile,
                installation,
                apply,
            } => {
                let root = match installation {
                    Some(root) => Some(std::path::absolute(paths::expand_home(root)?)?),
                    None => match paths::legacy_dirs()?.into_iter().next() {
                        Some(root) if paths::matching_installation(from, &root)? => Some(root),
                        _ => None,
                    },
                };
                if let Some(root) = &root {
                    anyhow::ensure!(root.is_dir(), "Original installation directory not found");
                }
                let plan = paths::plan_migration_with_installation(
                    from,
                    &data,
                    profile.as_deref(),
                    root.as_deref(),
                )?;
                if *apply {
                    plan.apply()?;
                }
                println!(
                    "{}",
                    serde_json::json!({"applied": apply, "migration": plan,
                    "activation": "Start Portal with --config pointing to the destination at the next controlled restart. Existing running instances and source files are unchanged."})
                );
                Ok(())
            }
        };
    }

    if matches!(&command, Some(Commands::Upgrade { status: true, .. })) {
        #[cfg(windows)]
        return windows_upgrade::show_status();
        #[cfg(target_os = "macos")]
        return macos_upgrade::show_status();
        #[cfg(not(any(windows, target_os = "macos")))]
        anyhow::bail!("Upgrade status is supported on Windows and macOS");
    }
    if cli.legacy_upgrade || matches!(&command, Some(Commands::Upgrade { .. })) {
        upgrade::ensure_standalone_upgrade()?;
    }
    if let Some(Commands::Upgrade {
        file: Some(path), ..
    }) = &command
    {
        #[cfg(windows)]
        return windows_upgrade::upgrade_file(path).await;
        #[cfg(target_os = "macos")]
        return macos_upgrade::handoff(&tokio::fs::read(path).await?, None).await;
        #[cfg(not(any(windows, target_os = "macos")))]
        anyhow::bail!("Local executable upgrades are supported on Windows and macOS");
    }
    if let Some(Commands::Upgrade {
        target: Some(path), ..
    }) = &command
    {
        #[cfg(target_os = "macos")]
        return macos_upgrade::migrate(path).await;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            anyhow::bail!("Installation migration is supported on macOS");
        }
    }
    if cli.legacy_upgrade || matches!(&command, Some(Commands::Upgrade { .. })) {
        return upgrade::run_upgrade().await;
    }
    if matches!(&command, Some(Commands::Stop | Commands::Status)) {
        #[cfg(windows)]
        return windows_start::run(
            if matches!(&command, Some(Commands::Stop)) {
                "stop"
            } else {
                "status"
            },
            None,
            None,
            None,
        )
        .await;
        #[cfg(target_os = "macos")]
        return macos_supervisor::action(if matches!(&command, Some(Commands::Stop)) {
            "stop"
        } else {
            "status"
        })
        .await;
        #[cfg(not(any(windows, target_os = "macos")))]
        anyhow::bail!("Use your OS service manager for start/stop/status on this platform");
    }
    #[cfg(windows)]
    if command.is_none() && std::env::var("HEART_PORTAL_SUPERVISED").as_deref() != Ok("1") {
        return windows_start::run(
            "start",
            cli.config.as_deref().or(cli.config_positional.as_deref()),
            cli.connect.as_deref(),
            cli.name.as_deref(),
        )
        .await;
    }
    #[cfg(target_os = "macos")]
    if command.is_none() {
        macos_upgrade::recover_interrupted()?;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,heart_portal=debug".parse().unwrap_or_else(|e| {
                    eprintln!("Failed to parse default log filter: {}", e);
                    tracing_subscriber::EnvFilter::new("info")
                })
            }),
        )
        .init();

    // Supervisors can provide the Loom link through the environment so the
    // credential is not exposed in the OS process command line.
    let mut connect_link = cli.connect.or_else(|| {
        std::env::var("PORTAL_CONNECT_LINK")
            .ok()
            .filter(|link| !link.trim().is_empty())
    });
    let explicit_config = cli
        .config
        .as_deref()
        .or(cli.config_positional.as_deref())
        .map(std::path::Path::new);
    let location = paths::locate_config(explicit_config, &paths::legacy_dirs()?)?;
    let config_path = location
        .path
        .to_str()
        .context("Config path must be Unicode")?
        .to_owned();
    info!("Config path: {} ({})", config_path, location.source);
    let cli_portal_name = cli.name;

    if !matches!(&command, Some(Commands::Kit { .. })) {
        paths::initialize_config(&location)?;
    }
    let config_loaded = PathBuf::from(&config_path).try_exists()?;
    let mut config = if config_loaded {
        PortalConfig::load(&config_path)?
    } else {
        info!("No config file at {}, using defaults", config_path);
        let mut defaults = PortalConfig::default();
        defaults.bind_host = "127.0.0.1".into();
        defaults.security.workspace_root = paths::data_dir()?.join("workspace");
        defaults
    };
    if let Some(enabled) = cli.exec_enabled {
        config.tools.exec = enabled;
    }
    if let Some(enabled) = cli.kits_enabled {
        config.kits_enabled = enabled;
        config.tools.custom_tools_enabled = enabled;
    }
    for warning in &config.warnings {
        warn!(
            "Config warning [{}] {}: {}",
            warning.code, warning.field, warning.message
        );
    }
    connect_link = connect_link.or_else(|| config.connect_link.clone());

    if let Ok(t) = std::env::var("PORTAL_MCP_TOKEN") {
        if !t.is_empty() {
            config.portal_mcp_token = Some(t);
        }
    }

    if let Some(Commands::Kit { command }) = &command {
        return match command {
            KitCommands::List => list_installed_kits(&config).await,
            KitCommands::Status => show_kit_status(&config).await,
        };
    }

    if matches!(&command, Some(Commands::SubagentSetup)) {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::io::stdin().take(16385).read_to_end(&mut bytes).await?;
        anyhow::ensure!(bytes.len() <= 16384, "Subagent configuration too large");
        let args: serde_json::Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(args.is_object(), "Expected a configuration object");
        // The explicit setup tool performs the one installation attempt.
        config.subagent.auto_install = false;
        let host = ToolHost::new_with_runtime(&config, tools::status::RuntimeStatus::capture(&config, location, config_loaded, config.name.clone(), false, runtime_started));
        let result = host.call("portal_subagent_setup", args).await?;
        println!("DESKTOP_SUBAGENT_RESULT={}", result);
        return Ok(());
    }

    // Prevent stale/duplicate Portal processes from competing for the
    // same relay connection and ejecting one another. Management subcommands
    // above remain usable while a Portal instance is running.
    // Key the instance guard by relay host + Being rather than by the whole
    // Loom URL. Rotating a token must not allow a second local instance to
    // bypass the duplicate-process guard.
    #[cfg(windows)]
    if windows_upgrade::recover_interrupted()? {
        return Ok(());
    }
    #[cfg(windows)]
    let startup_guard = windows_upgrade::startup_guard()?;
    #[cfg(target_os = "macos")]
    let startup_guard = macos_upgrade::startup_guard()?;
    let instance_identity = match connect_link.as_deref() {
        Some(link) => {
            let (host, being_id, _) = relay_client::parse_loom_link(link)?;
            format!("{}/{being_id}", host.to_ascii_lowercase())
        }
        None => format!("standalone/{}:{}", config.bind_host, config.bind_port),
    };
    let _single_instance =
        single_instance::acquire(Some(&instance_identity)).map_err(|e| anyhow::anyhow!(e))?;

    config.prepare_workspace()?;
    info!(
        "Workspace ready: {}",
        config.security.workspace_root.display()
    );

    #[cfg(target_os = "macos")]
    macos_supervisor::start(
        &config_path,
        connect_link.as_deref(),
        cli_portal_name.as_deref(),
    )
    .await?;

    if config.portal_mcp_token.is_none() {
        warn!("PORTAL_MCP_TOKEN is not set — MCP TCP connections are unauthenticated (set token for public deployments)");
    }

    if connect_link.is_some() {
        info!(
            "Portal '{}' connect mode — MCP via Hearth relay",
            config.name
        );
    } else {
        info!(
            "Portal '{}' starting on {}:{}",
            config.name, config.bind_host, config.bind_port
        );
    }

    // Resolve the advertised identity once; diagnostics and the relay handshake
    // must describe the same running instance, including CLI/environment overrides.
    let effective_name = if connect_link.is_some() {
        relay_portal_name(cli_portal_name, &config.name, default_relay_portal_name)
    } else {
        config.name.clone()
    };
    let runtime = tools::status::RuntimeStatus::capture(
        &config,
        location,
        config_loaded,
        effective_name.clone(),
        connect_link.is_some(),
        runtime_started,
    );
    let mut tool_host = ToolHost::new_with_runtime(&config, runtime);
    if let (Ok(file), Some(link)) = (
        std::env::var("HEART_PORTAL_CLIENT_FILE"),
        connect_link.as_deref(),
    ) {
        if let Ok(mut endpoint) = url::Url::parse(link) {
            endpoint.set_query(None);
            endpoint.set_fragment(None);
            tool_host = tool_host.with_client_handler(std::sync::Arc::new(
                tools::client::DesktopClientHandler {
                    file: file.into(),
                    endpoint: endpoint.as_str().trim_end_matches('/').to_string(),
                },
            ));
        }
    }

    if config.kits_enabled {
        tool_host.start_kit_refresh_task();
        info!("Kit manifest and .env hot-reload started (every 5s)");
    }

    let cleanup_host = tool_host.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            cleanup_host.cleanup_background_sessions().await;
        }
    });

    let tool_list = tool_host.list_tools().await;
    info!(
        "Portal tools: {}",
        tool_list
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Eager kits are optional. Start warming them after initial tool discovery
    // without holding up listener/relay readiness or the upgrade deadline.
    // Cancel warmup before shutdown so it cannot spawn kits after cleanup.
    let mut warmup = tokio::task::JoinSet::new();
    let warmup_host = tool_host.clone();
    warmup.spawn(async move {
        warmup_host.warmup_kits().await;
    });
    let custom_host = tool_host.clone();
    warmup.spawn(async move {
        match custom_host.load_custom_tools().await {
            Ok(count) => info!("Loaded {} custom MCP tools", count),
            Err(_) => warn!("Custom MCP startup failed; Portal remains available"),
        }
    });

    if let Some(ref loom) = connect_link {
        let relay_portal_name = effective_name;

        // Async callback: finished background sessions POST back to the being's
        // Heart, which writes the inbox and triggers breathe_callback.
        match relay_client::parse_loom_link(loom) {
            Ok((host, being_id, token)) => {
                let url = callback_url(loom, &host, &being_id);
                tool_host.set_callback_config(url, token, relay_portal_name.clone());
            }
            Err(e) => warn!("async callback disabled (invalid Loom link): {e:#}"),
        }

        publish_supervisor_ready()?;
        #[cfg(any(windows, target_os = "macos"))]
        drop(startup_guard);
        let tool_shutdown = tool_host.clone();
        let restart_waiter = tool_host.clone();
        tokio::select! {
            _ = async {
                let _ = tokio::signal::ctrl_c().await;
            } => {
                info!("Portal shutting down (Ctrl+C)");
                #[cfg(target_os = "macos")]
                macos_supervisor::stop_on_interrupt();
                warmup.shutdown().await;
                tool_shutdown.kill_all_managed_processes().await;
            }
            _ = wait_sigterm() => {
                info!("Portal shutting down (termination signal)");
                warmup.shutdown().await;
                tool_shutdown.kill_all_managed_processes().await;
            }
            _ = restart_waiter.wait_for_restart() => {
                info!("Portal restarting after a controlled tool request");
                warmup.shutdown().await;
                tool_shutdown.kill_all_managed_processes().await;
            }
            _ = relay_client::connect_and_serve(loom, &tool_host, &relay_portal_name) => {}
        }
        return Ok(());
    }

    // Standalone mode has no callback target, but the ledger still needs to
    // close out sub-agent tasks orphaned by the previous run. In --connect mode
    // this runs from set_callback_config instead, once there is an inbox to
    // deliver the `interrupted` results to.
    tool_host.subagent.reconcile().await;

    // Track active connections
    let active_connections = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Listen for MCP supervisor connections
    let addr = format!("{}:{}", config.bind_host, config.bind_port);
    let listener = TcpListener::bind(&addr).await?;
    info!("Portal MCP listening on {}", addr);
    tool_host.set_connection_state(tools::status::ConnectionState::Listening);
    connection_status::publish("local");
    publish_supervisor_ready()?;
    #[cfg(any(windows, target_os = "macos"))]
    drop(startup_guard);

    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let mut shutdown_rx = shutdown_tx.subscribe();
    let shutdown_cleanup = tool_host.clone();
    let restart_waiter = tool_host.clone();
    tokio::spawn({
        let shutdown_tx = shutdown_tx.clone();
        async move {
            tokio::select! {
                r = tokio::signal::ctrl_c() => {
                    let _ = r;
                    #[cfg(target_os = "macos")]
                    macos_supervisor::stop_on_interrupt();
                }
                _ = wait_sigterm() => {}
                _ = restart_waiter.wait_for_restart() => {
                    info!("Portal restarting after a controlled tool request");
                }
            }
            info!("Portal shutting down");
            let _ = shutdown_tx.send(());
        }
    });
    drop(shutdown_tx);

    loop {
        tokio::select! {
            biased;
            res = shutdown_rx.recv() => {
                match res {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
                warmup.shutdown().await;
                shutdown_cleanup.kill_all_managed_processes().await;
                break;
            }
            accept = listener.accept() => {
                let (stream, peer) = accept.context("MCP listener accept failed")?;
                let conn_count = active_connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                info!("MCP client connected from {} (active: {})", peer, conn_count);

                let tool_host = tool_host.clone();
                let portal_name = config.name.clone();
                let mcp_token = config.portal_mcp_token.clone();
                let active = active_connections.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &tool_host, &portal_name, mcp_token.as_deref()).await {
                        warn!("Connection from {} ended: {}", peer, e);
                    } else {
                        info!("Connection from {} closed cleanly", peer);
                    }
                    let remaining = active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1;
                    info!("Connection closed (active: {})", remaining);
                });
            }
        }
    }

    drop(listener);
    Ok(())
}

/// Local readiness is independent of relay availability: a network outage must
/// not turn a working binary into a failed upgrade. The supervisor checks the
/// PID, a fresh per-launch nonce, and the process creation time as well.
fn publish_supervisor_ready() -> Result<()> {
    #[cfg(windows)]
    windows_upgrade::publish_ready()?;
    #[cfg(target_os = "macos")]
    macos_upgrade::publish_ready()?;
    Ok(())
}

async fn list_installed_kits(config: &PortalConfig) -> Result<()> {
    if !config.kits_enabled {
        println!("Kits disabled");
        return Ok(());
    }

    let kits_dir = kits::loader::kits_dir(config);
    let kits = kits::loader::load_kits(config)?;
    if kits.is_empty() {
        println!("No kits installed in {}", kits_dir.display());
        return Ok(());
    }

    let manager = kits::manager::KitManager::new(kits);
    println!("Installed kits in {}", kits_dir.display());
    for status in manager.statuses().await {
        println!(
            "{}\t{}\t{}\t{} tool(s)",
            status.name, status.version, status.status, status.tools
        );
    }

    Ok(())
}

async fn show_kit_status(config: &PortalConfig) -> Result<()> {
    if !config.kits_enabled {
        println!("Kits disabled");
        return Ok(());
    }

    let kits_dir = kits::loader::kits_dir(config);
    let kits = kits::loader::load_kits_from_dir(&kits_dir)?;
    if kits.is_empty() {
        println!("No kits installed in {}", kits_dir.display());
        return Ok(());
    }

    println!(
        "{:<16} {:<8} {:<6} {:<11} {}",
        "Kit", "Version", "Tools", "Status", "Command"
    );
    for kit in kits {
        let status = if kit.configuration_error().is_some() {
            "needs-configuration"
        } else if kits::loader::command_binary_exists(&kit.command) {
            "not-started"
        } else {
            "unhealthy"
        };
        println!(
            "{:<16} {:<8} {:<6} {:<11} {}",
            &kit.manifest.name,
            &kit.manifest.version,
            kit.manifest.tools.len(),
            status,
            kits::loader::format_command(&kit.command)
        );
        if let Some(error) = kit.configuration_error() {
            println!(
                "  {}. Inspect portal_kits_setup for this kit (env: {})",
                error,
                kit.kit_dir.join(".env").display()
            );
        }
    }

    Ok(())
}

/// Derive Heart's callback endpoint from the Loom link.
/// `https://echo.beings.town/alice/?token=…` → `https://echo.beings.town/alice/api/callback`.
/// Scheme follows the Loom link, except localhost/127.* which is always plain http.
fn callback_url(loom_link: &str, host: &str, being_id: &str) -> String {
    let is_localhost = relay_client::is_loopback_host(host);
    let scheme = if is_localhost {
        "http"
    } else if loom_link.trim_start().starts_with("http://") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}/{being_id}/api/callback")
}

fn default_relay_portal_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "portal".to_string())
}

fn relay_portal_name(
    cli_portal_name: Option<String>,
    config_name: &str,
    fallback: impl FnOnce() -> String,
) -> String {
    cli_portal_name
        .or_else(|| {
            if !config_name.is_empty() && config_name != "portal" {
                Some(config_name.to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(fallback)
}

#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            warn!(
                "Failed to install SIGTERM handler: {}; falling back to Ctrl+C",
                e
            );
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(windows)]
async fn wait_sigterm() {
    match tokio::signal::windows::ctrl_break() {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            warn!(
                "Failed to install CTRL_BREAK handler: {}; falling back to Ctrl+C",
                e
            );
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(any(unix, windows)))]
async fn wait_sigterm() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Handle a single MCP client connection (JSON-RPC over newline-delimited TCP)
pub(crate) async fn handle_connection<S>(
    stream: S,
    tool_host: &ToolHost,
    portal_name: &str,
    expected_token: Option<&str>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);
    let mut auth_bytes = Vec::new();

    if let Some(expected) = expected_token.filter(|t| !t.is_empty()) {
        loop {
            auth_bytes.clear();
            let bytes_read = tokio::time::timeout(
                Duration::from_secs(10),
                crate::mcp::limits::read_line_append(&mut reader, &mut auth_bytes, 64 * 1024),
            )
            .await
            .context("MCP authentication timed out")??;
            let line = std::str::from_utf8(&auth_bytes)?;
            if bytes_read == 0 {
                debug!("Client disconnected before auth (EOF)");
                return Ok(());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            debug!("Received MCP authentication message");

            let value: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: None,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32700,
                            message: format!("Parse error: {}", e),
                            data: None,
                        }),
                    };
                    send_response(&mut writer, &error_resp).await?;
                    anyhow::bail!("MCP auth: invalid JSON");
                }
            };

            let method = value
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    debug!("Missing or invalid 'method' field in JSON-RPC request");
                    ""
                });
            let id = value.get("id").cloned();
            if method != "auth" {
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.as_ref().and_then(|v| v.as_u64()),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32001,
                        message: "Unauthorized: first message must be {\"method\":\"auth\",\"params\":{\"token\":\"...\"}}"
                            .to_string(),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                anyhow::bail!("MCP auth: expected auth as first message");
            }

            let token = value
                .get("params")
                .and_then(|p| p.get("token"))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    debug!("Missing or invalid token in auth params");
                    ""
                });
            if !constant_time_token_matches(&token, &expected) {
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.as_ref().and_then(|v| v.as_u64()),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32002,
                        message: "Unauthorized: invalid token".to_string(),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                anyhow::bail!("MCP auth: invalid token");
            }

            let ok = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: id.as_ref().and_then(|v| v.as_u64()),
                result: Some(serde_json::json!({ "authenticated": true })),
                error: None,
            };
            send_response(&mut writer, &ok).await?;
            break;
        }
    }

    let mut changes = tool_host.subscribe_tools_changed();
    let mut initialized = false;
    // Keep bounded work and management capacity separate on a single relay/TCP
    // connection. A slow community call must not serialize every Being request.
    let mut calls = tokio::task::JoinSet::<(JsonRpcResponse, bool)>::new();
    let mut management = tokio::task::JoinSet::<(JsonRpcResponse, bool)>::new();
    let mut active_requests = std::collections::HashMap::<u64, tokio::task::AbortHandle>::new();
    // Partial reads survive either a completed request or a change notification.
    let mut request_bytes = Vec::new();
    loop {
        let bytes_read = tokio::select! {
            Some(result) = management.join_next_with_id(), if !management.is_empty() => {
                match result {
                    Ok((task_id, (response, restart))) => {
                        if response.id.and_then(|id| active_requests.get(&id)).is_some_and(|h| h.id() == task_id) {
                            active_requests.remove(&response.id.unwrap());
                            send_response(&mut writer, &response).await?;
                            if restart { tool_host.restart_after_response(); }
                        }
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => return Err(error.into()),
                }
                continue;
            }
            Some(result) = calls.join_next_with_id(), if !calls.is_empty() => {
                match result {
                    Ok((task_id, (response, _))) => {
                        if response.id.and_then(|id| active_requests.get(&id)).is_some_and(|h| h.id() == task_id) {
                            active_requests.remove(&response.id.unwrap());
                            send_response(&mut writer, &response).await?;
                        }
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => return Err(error.into()),
                }
                continue;
            }
            result = crate::mcp::limits::read_line_append(&mut reader, &mut request_bytes, 16 * 1024 * 1024) => result?,
            result = changes.changed(), if initialized => {
                result?;
                send_notification(&mut writer, &serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                    "params": {}
                })).await?;
                continue;
            }
        };
        if bytes_read == 0 {
            debug!("Client disconnected (EOF)");
            return Ok(());
        }

        let line = String::from_utf8(std::mem::take(&mut request_bytes))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        debug!("Received MCP message ({} bytes)", trimmed.len());

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "Invalid JSON-RPC at line {}, column {}",
                    e.line(),
                    e.column()
                );
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                continue;
            }
        };

        if request.id.is_none() {
            if request.method == "notifications/cancelled" {
                if let Some(id) = request.params.get("requestId").and_then(|v| v.as_u64()) {
                    if let Some(task) = active_requests.remove(&id) {
                        task.abort();
                    }
                }
            }
            continue;
        }
        if active_requests.contains_key(&request.id.unwrap()) {
            let response = JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32600,
                    message: "Request ID is already in progress".into(),
                    data: None,
                }),
            };
            send_response(&mut writer, &response).await?;
            continue;
        }

        if request.method == "initialize" {
            // Publish initialization before permitting list-change notifications.
            send_response(
                &mut writer,
                &handle_request(&request, tool_host, portal_name).await,
            )
            .await?;
            initialized = true;
            continue;
        }
        let name = request
            .params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_management = matches!(request.method.as_str(), "ping" | "tools/list")
            || (request.method == "tools/call"
                && matches!(
                    name,
                    "portal_status"
                        | "portal_kits_status"
                        | "portal_kits_setup"
                        | "portal_kits_reload"
                        | "portal_restart"
                ));
        let restart = request.method == "tools/call" && name == "portal_restart";
        let tasks = if is_management {
            &mut management
        } else {
            &mut calls
        };
        let limit = if is_management {
            mcp::limits::MANAGEMENT_REQUESTS
        } else {
            mcp::limits::WORK_REQUESTS
        };
        if tasks.len() >= limit {
            let response = JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32000,
                    message: "Portal connection is busy; wait for pending requests before retrying"
                        .into(),
                    data: None,
                }),
            };
            send_response(&mut writer, &response).await?;
            continue;
        }
        let host = tool_host.clone();
        let name = portal_name.to_string();
        let id = request.id.unwrap();
        let handle =
            tasks.spawn(async move { (handle_request(&request, &host, &name).await, restart) });
        active_requests.insert(id, handle);
    }
}

/// Route a JSON-RPC request to the appropriate handler
async fn handle_request(
    request: &JsonRpcRequest,
    tool_host: &ToolHost,
    portal_name: &str,
) -> JsonRpcResponse {
    let id = request.id;

    match request.method.as_str() {
        "initialize" => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": true }
                },
                "serverInfo": {
                    "name": format!("heart-portal-{}", portal_name),
                    "version": PORTAL_VERSION
                }
            })),
            error: None,
        },

        "tools/list" => {
            let tools: Vec<serde_json::Value> = tool_host
                .list_tools()
                .await
                .iter()
                .map(|t| {
                    let mut tool = serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema
                    });
                    if matches!(
                        t.name.as_str(),
                        "portal_status" | "portal_kits_status" | "portal_kits_setup"
                    ) {
                        tool["annotations"] = serde_json::json!({"readOnlyHint": true});
                    }
                    tool
                })
                .collect();

            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({ "tools": tools })),
                error: None,
            }
        }

        "tools/call" => {
            let tool_name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    debug!("Missing or invalid tool name in tools/call request");
                    ""
                });
            let arguments = match request.params.get("arguments") {
                Some(value) if value.is_object() => value.clone(),
                None => serde_json::json!({}),
                Some(_) => {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".into(),
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32602,
                            message: "Tool arguments must be an object".into(),
                            data: None,
                        }),
                    }
                }
            };

            let start = std::time::Instant::now();
            info!("⚡ {} called", tool_name);

            let scene_id = request
                .params
                .get("_meta")
                .or_else(|| request.params.get("meta"))
                .or(Some(&request.meta))
                .and_then(|meta| meta.get("scene_id"))
                .and_then(serde_json::Value::as_str);
            let result = tool_host
                .call_with_scene(tool_name, arguments, scene_id)
                .await;
            let elapsed = start.elapsed();

            match result {
                Ok(value) => {
                    let is_error = value.get("isError").and_then(|v| v.as_bool()).unwrap_or_else(|| {
                        if value.get("content").is_none() {
                            warn!("Tool '{}' returned a malformed result with no content and no valid isError field; assuming success", tool_name);
                        } else {
                            trace!("Missing or invalid isError field in tool result, assuming success");
                        }
                        false
                    });
                    if is_error {
                        warn!("⚡ {} → error ({:?})", tool_name, elapsed);
                    } else {
                        info!("⚡ {} → ok ({:?})", tool_name, elapsed);
                    }
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(value),
                        error: None,
                    }
                }
                Err(e) => {
                    warn!(
                        "⚡ {} → fail ({:?}); details returned to caller",
                        tool_name, elapsed
                    );
                    let message = format!("Tool error: {}", e);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(serde_json::json!({
                            "content": [{"type": "text", "text": message}],
                            "isError": true
                        })),
                        error: None,
                    }
                }
            }
        }

        "ping" => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(serde_json::json!({})),
            error: None,
        },

        _ => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("Method not found: {}", request.method),
                data: None,
            }),
        },
    }
}

/// Send a JSON-RPC response (newline-delimited)
async fn send_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    response: &JsonRpcResponse,
) -> Result<()> {
    let json = serde_json::to_string(response)?;
    debug!("Sending MCP response ({} bytes)", json.len());
    tokio::time::timeout(Duration::from_secs(10), async {
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok::<_, std::io::Error>(())
    })
    .await
    .context("MCP client stopped reading responses")??;
    Ok(())
}

/// Send a JSON-RPC notification (no id, no response expected).
async fn send_notification<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    notification: &serde_json::Value,
) -> Result<()> {
    let json = serde_json::to_string(notification)?;
    debug!("Sending MCP notification ({} bytes)", json.len());
    tokio::time::timeout(Duration::from_secs(10), async {
        writer.write_all(json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok::<_, std::io::Error>(())
    })
    .await
    .context("MCP client stopped reading responses")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_portal_name_prefers_cli_name() {
        let name = relay_portal_name(Some("foo".to_string()), "cotton", || "host".to_string());
        assert_eq!(name, "foo");
    }

    #[test]
    fn relay_portal_name_uses_non_generic_config_name() {
        let name = relay_portal_name(None, "cotton", || "host".to_string());
        assert_eq!(name, "cotton");
    }

    #[test]
    fn relay_portal_name_skips_generic_config_name() {
        let name = relay_portal_name(None, "portal", || "host".to_string());
        assert_eq!(name, "host");
    }

    #[test]
    fn relay_portal_name_skips_empty_config_name() {
        let name = relay_portal_name(None, "", || "host".to_string());
        assert_eq!(name, "host");
    }

    #[test]
    fn callback_url_follows_loom_scheme() {
        assert_eq!(
            callback_url(
                "https://echo.beings.town/alice/?token=abc",
                "echo.beings.town",
                "alice"
            ),
            "https://echo.beings.town/alice/api/callback"
        );
    }

    #[test]
    fn callback_url_uses_http_for_localhost() {
        assert_eq!(
            callback_url(
                "https://localhost:3100/hex/?token=abc",
                "localhost:3100",
                "hex"
            ),
            "http://localhost:3100/hex/api/callback"
        );
        assert_eq!(
            callback_url(
                "http://127.0.0.1:3100/hex/?token=abc",
                "127.0.0.1:3100",
                "hex"
            ),
            "http://127.0.0.1:3100/hex/api/callback"
        );
    }

    #[test]
    fn callback_url_keeps_plain_http_for_remote_http_loom() {
        assert_eq!(
            callback_url(
                "http://box.local:8080/bee/?token=abc",
                "box.local:8080",
                "bee"
            ),
            "http://box.local:8080/bee/api/callback"
        );
    }

    #[test]
    fn callback_url_never_carries_the_token() {
        let url = callback_url(
            "https://echo.beings.town/alice/?token=supersecret",
            "echo.beings.town",
            "alice",
        );
        assert!(!url.contains("supersecret"));
        assert!(!url.contains('?'));
    }
}

// Hash both strings to fixed-size values before the constant-time comparison.
// Token length need not be secret; token contents must not determine comparison time.
fn constant_time_token_matches(actual: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    bool::from(Sha256::digest(actual.as_bytes()).ct_eq(&Sha256::digest(expected.as_bytes())))
}

#[cfg(test)]
#[test]
fn token_comparison_accepts_only_equal_tokens() {
    assert!(constant_time_token_matches("test-token", "test-token"));
    assert!(!constant_time_token_matches("test-token", "test-tokee"));
    assert!(!constant_time_token_matches("test-token", "test-token-long"));
}

#[cfg(test)]
#[test]
fn debug_configurations_redact_credentials_and_command_arguments() {
    let mut config = config::PortalConfig::default();
    config.connect_link = Some("synthetic-link-secret".into());
    config.portal_mcp_token = Some("synthetic-mcp-secret".into());
    assert!(!format!("{config:?}").contains("synthetic-"));
    let server = mcp::McpServerConfig { name: "fixture".into(),
        command: vec!["synthetic-command-secret".into()],
        env: std::collections::HashMap::from([("TOKEN".into(), "synthetic-env-secret".into())]), cwd: None };
    assert!(!format!("{server:?}").contains("synthetic-"));
}
