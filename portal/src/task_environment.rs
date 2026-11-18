//! A stable, credential-free layout shared by foreground and background sandboxes.
use crate::config::PortalConfig;
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Release-owned interpreters live in the client-managed shared runtime directory.
#[derive(serde::Deserialize)]
pub(crate) struct Interpreter {
    pub version: String,
    pub path: PathBuf,
}
#[derive(serde::Deserialize)]
struct Manifest {
    schema: u32,
    platform: String,
    arch: String,
    python: Interpreter,
    node: Interpreter,
}
pub(crate) struct Environment {
    pub variables: BTreeMap<String, String>,
    pub runtime: Option<Runtime>,
}
pub(crate) struct Runtime {
    pub root: PathBuf,
    pub python: Interpreter,
    pub node: Interpreter,
}

pub(crate) fn bundled_runtime() -> Result<Option<Runtime>> {
    let root = if let Some(path) = std::env::var_os("HEART_PORTAL_RUNTIME_DIR") {
        PathBuf::from(path)
    } else {
        std::env::current_exe()?
            .parent()
            .context("Portal executable has no directory")?
            .join("portal-runtime")
    };
    let runtime = load_runtime(&root)?;
    anyhow::ensure!(runtime.is_some() || std::env::var("HEART_PORTAL_CLIENT_MANAGED").as_deref() != Ok("1"),
        "Bundled Python/Node runtime is missing; repair or reinstall this Portal package. Do not search host interpreters or generate a launcher.");
    Ok(runtime)
}

fn load_runtime(root: &Path) -> Result<Option<Runtime>> {
    let text = match std::fs::read_to_string(root.join("runtime.json")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Read bundled runtime manifest"),
    };
    let manifest: Manifest =
        serde_json::from_str(&text).context("Invalid bundled runtime manifest")?;
    let platform = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(windows) {
        "win32"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    anyhow::ensure!(
        manifest.schema == 1 && manifest.platform == platform && manifest.arch == arch,
        "Bundled runtime does not match this platform; reinstall the correct Portal package"
    );
    let root = root.canonicalize()?;
    for interpreter in [&manifest.python, &manifest.node] {
        anyhow::ensure!(
            interpreter
                .path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
                && root.join(&interpreter.path).is_file(),
            "Bundled interpreter is missing or has an invalid path; repair the Portal package"
        );
    }
    Ok(Some(Runtime {
        root,
        python: manifest.python,
        node: manifest.node,
    }))
}

pub(crate) fn prepare(config: &PortalConfig) -> Result<Environment> {
    let runtime = bundled_runtime()?;
    let variables = prepare_environment(config, runtime.as_ref())?;
    Ok(Environment { variables, runtime })
}

fn prepare_environment(
    config: &PortalConfig,
    bundled: Option<&Runtime>,
) -> Result<BTreeMap<String, String>> {
    let root = super::tools::file::canonical_workspace_root(config)?;
    let runtime = root.join(".portal-runtime");
    let mut env = BTreeMap::new();
    for (key, relative) in [
        ("HOME", "home"),
        ("USERPROFILE", "home"),
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_CONFIG_HOME", "config"),
        ("LOCALAPPDATA", "cache"),
        ("APPDATA", "config"),
        ("PYTHONPYCACHEPREFIX", "cache/python"),
        ("PYTHONUSERBASE", "python-user"),
        ("MPLCONFIGDIR", "cache/matplotlib"),
        ("PIP_CACHE_DIR", "cache/pip"),
        ("UV_CACHE_DIR", "cache/uv"),
        ("npm_config_cache", "cache/npm"),
    ] {
        let path = runtime.join(relative);
        // Use the file-tool boundary, including symlink and protected-path checks.
        let path = super::tools::file::resolve_write_path(config, &path.to_string_lossy())?;
        super::tools::file::authorize_write(config, &path)?;
        std::fs::create_dir_all(&path)?;
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
    for relative in ["inputs", "outputs"] {
        let path =
            super::tools::file::resolve_write_path(config, &root.join(relative).to_string_lossy())?;
        super::tools::file::authorize_write(config, &path)?;
        std::fs::create_dir_all(path)?;
    }
    let mut paths = Vec::new();
    if let Some(runtime) = bundled {
        paths.push(
            runtime
                .root
                .join(&runtime.python.path)
                .parent()
                .unwrap()
                .to_owned(),
        );
        paths.push(
            runtime
                .root
                .join(&runtime.node.path)
                .parent()
                .unwrap()
                .to_owned(),
        );
    }
    // Project executables remain available after the release-owned interpreters.
    #[cfg(unix)]
    paths.extend([root.join(".venv/bin"), root.join("node_modules/.bin")]);
    #[cfg(target_os = "macos")]
    paths.extend(
        [
            "/usr/local/bin",
            "/Library/Developer/CommandLineTools/usr/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .map(PathBuf::from),
    );
    #[cfg(windows)]
    {
        paths.push(root.join(".venv/Scripts"));
        let system = PathBuf::from(
            std::env::var_os("SystemRoot")
                .ok_or_else(|| anyhow::anyhow!("SystemRoot unavailable"))?,
        );
        paths.extend([
            system.join("System32"),
            system.clone(),
            system.join("System32/WindowsPowerShell/v1.0"),
        ]);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    paths.extend(["/usr/bin", "/bin"].map(PathBuf::from));
    env.insert(
        "PATH".into(),
        std::env::join_paths(paths)?.to_string_lossy().into_owned(),
    );
    env.insert("PYTHONIOENCODING".into(), "utf-8".into());
    env.insert("PYTHONUTF8".into(), "1".into());
    env.insert("LANG".into(), "en_US.UTF-8".into());
    env.insert(
        "PORTAL_INPUTS".into(),
        root.join("inputs").to_string_lossy().into_owned(),
    );
    env.insert(
        "PORTAL_OUTPUTS".into(),
        root.join("outputs").to_string_lossy().into_owned(),
    );
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prepares_reusable_writable_directories_without_host_credentials() {
        let root = std::env::temp_dir().join(format!("portal-env-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = PortalConfig::default();
        config.security.workspace_root = root.clone();
        let first = prepare(&config).unwrap().variables;
        assert_eq!(first, prepare(&config).unwrap().variables);
        for key in [
            "HOME",
            "TMPDIR",
            "MPLCONFIGDIR",
            "PIP_CACHE_DIR",
            "PORTAL_INPUTS",
            "PORTAL_OUTPUTS",
        ] {
            assert!(std::path::Path::new(&first[key]).is_dir());
            assert!(std::path::Path::new(&first[key]).starts_with(root.canonicalize().unwrap()));
        }
        assert!(!first.contains_key("PORTAL_TOKEN"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn refuses_runtime_symlink_outside_workspace() {
        let root = std::env::temp_dir().join(format!("portal-env-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join(".portal-runtime")).unwrap();
        let mut config = PortalConfig::default();
        config.security.permission_mode = crate::config::PermissionMode::Supervised;
        config.security.workspace_root = root.clone();
        config.prepare_workspace().unwrap();
        assert!(prepare(&config).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
