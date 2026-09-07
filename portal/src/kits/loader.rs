use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, warn};

use crate::config::PortalConfig;

use super::manifest::KitManifest;

#[derive(Debug, Clone)]
pub struct LoadedKit {
    pub manifest: KitManifest,
    pub kit_dir: PathBuf,
    pub command: Vec<String>,
}

pub fn load_kits(config: &PortalConfig) -> Result<Vec<LoadedKit>> {
    if !config.kits_enabled {
        debug!("Kits disabled in configuration");
        return Ok(Vec::new());
    }

    load_kits_from_dir(&kits_dir(config))
}

pub fn load_kits_from_dir(kits_dir: &Path) -> Result<Vec<LoadedKit>> {
    if !kits_dir.exists() {
        debug!("No kits directory at {}", kits_dir.display());
        return Ok(Vec::new());
    }

    let mut entries = std::fs::read_dir(kits_dir)
        .with_context(|| format!("Reading kits directory {}", kits_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("Reading entries from kits directory {}", kits_dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    let mut kits = Vec::new();
    for entry in entries {
        let kit_dir = entry.path();
        if !kit_dir.is_dir() {
            continue;
        }

        let manifest_path = kit_dir.join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }

        match load_manifest(&kit_dir, &manifest_path) {
            Ok(Some(kit)) => kits.push(kit),
            Ok(None) => {}
            Err(err) => {
                warn!(
                    "Skipping kit manifest {}: {}",
                    manifest_path.display(),
                    err
                );
            }
        }
    }

    Ok(kits)
}

fn load_manifest(kit_dir: &Path, manifest_path: &Path) -> Result<Option<LoadedKit>> {
    let content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("Reading kit manifest {}", manifest_path.display()))?;
    let manifest: KitManifest = serde_json::from_str(&content)
        .with_context(|| format!("Parsing kit manifest {}", manifest_path.display()))?;

    if !platform_matches(&manifest) {
        debug!(
            "Skipping kit '{}' because it does not support {}",
            manifest.name,
            current_platform()
        );
        return Ok(None);
    }

    let command = resolve_command(kit_dir, &manifest.command);
    for warning in manifest_validation_warnings(&manifest, &command) {
        warn!("{}", warning);
    }

    Ok(Some(LoadedKit {
        manifest,
        kit_dir: kit_dir.to_path_buf(),
        command,
    }))
}

fn platform_matches(manifest: &KitManifest) -> bool {
    let Some(platforms) = &manifest.platform else {
        return true;
    };

    let current = current_platform();
    platforms.iter().any(|platform| {
        let normalized = platform.to_ascii_lowercase();
        normalized == current || (current == "darwin" && normalized == "macos")
    })
}

pub fn current_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        std::env::consts::OS
    }
}

fn resolve_command(kit_dir: &Path, command: &[String]) -> Vec<String> {
    let mut resolved = command.to_vec();
    if resolved.is_empty() {
        return resolved;
    }

    let first = Path::new(&resolved[0]);

    if first.is_absolute() {
        if let Some(program) = find_binary_at(first) {
            resolved[0] = program.to_string_lossy().into_owned();
        }
        return resolved;
    }

    let candidate = kit_dir.join(first);
    let has_path_separator = resolved[0].contains('/') || resolved[0].contains('\\');
    if let Some(program) = find_binary_at(&candidate) {
        resolved[0] = program.to_string_lossy().into_owned();
        return resolved;
    }
    if has_path_separator {
        resolved[0] = candidate.to_string_lossy().to_string();
        return resolved;
    }

    #[cfg(windows)]
    if !has_path_separator {
        if let Some(program) = find_binary_on_path(&resolved[0]) {
            resolved[0] = program.to_string_lossy().to_string();
        }
    }

    resolved
}

pub(crate) fn manifest_validation_warnings(
    manifest: &KitManifest,
    command: &[String],
) -> Vec<String> {
    let mut warnings = Vec::new();
    let kit = kit_label(&manifest.name);

    if !is_valid_kit_name(&manifest.name) {
        warnings.push(format!(
            "kit {} has invalid name; expected non-empty ASCII alphanumeric characters and hyphens only",
            kit
        ));
    }

    if command.is_empty() {
        warnings.push(format!("kit {} command is empty", kit));
    } else if !command_binary_exists(command) {
        warnings.push(format!(
            "kit {} command binary not found: {}",
            kit, command[0]
        ));
    }

    if manifest.tools.is_empty() {
        warnings.push(format!("kit {} has no tools defined", kit));
    }

    warnings
}

pub(crate) fn is_valid_kit_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
}

pub(crate) fn command_binary_exists(command: &[String]) -> bool {
    let Some(binary) = command.first().filter(|binary| !binary.trim().is_empty()) else {
        return false;
    };

    binary_exists(binary)
}

pub(crate) fn format_command(command: &[String]) -> String {
    if command.is_empty() {
        "<empty>".to_string()
    } else {
        command.join(" ")
    }
}

fn binary_exists(binary: &str) -> bool {
    let path = Path::new(binary);
    if path.is_absolute() || binary.contains('/') || binary.contains('\\') {
        return find_binary_at(path).is_some();
    }

    find_binary_on_path(binary).is_some()
}

fn find_binary_on_path(binary: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        if let Some(program) = find_binary_at(&dir.join(binary)) {
            return Some(program);
        }
    }
    None
}

fn find_binary_at(path: &Path) -> Option<PathBuf> {
    // npm installs both a POSIX `codex` shim and `codex.cmd`. On Windows,
    // selecting the extensionless file first fails with Win32 error 193.
    #[cfg(windows)]
    if path.extension().is_none() {
        for ext in std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|ext| !ext.is_empty())
        {
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(ext);
            let candidate = PathBuf::from(candidate);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        return None;
    }
    path.is_file().then(|| path.to_path_buf())
}

fn kit_label(name: &str) -> String {
    if name.is_empty() {
        "'<unnamed>'".to_string()
    } else {
        format!("'{}'", name)
    }
}

pub fn kits_dir(config: &PortalConfig) -> PathBuf {
    expand_home(
        config
            .kits_dir
            .as_deref()
            .unwrap_or("~/.heart-portal/kits/"),
    )
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }

    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }

    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(|| {
            // Fallback for systemd services where $HOME is not set
            #[cfg(unix)]
            {
                // SAFETY: getuid is always safe
                let uid = unsafe { libc::getuid() };
                // SAFETY: getpwuid returns a pointer to a static struct or null
                let pw = unsafe { libc::getpwuid(uid) };
                if !pw.is_null() {
                    let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
                    if let Ok(s) = dir.to_str() {
                        return Some(PathBuf::from(s));
                    }
                }
                None
            }
            #[cfg(windows)]
            {
                None
            }
            #[cfg(not(any(unix, windows)))]
            {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::manifest::KitToolDef;

    #[test]
    fn filters_unsupported_platforms() {
        let manifest = KitManifest {
            name: "hand".to_string(),
            version: "0.1.0".to_string(),
            description: None,
            author: None,
            platform: Some(vec!["definitely-not-this-platform".to_string()]),
            runtime: None,
            command: vec!["python3".to_string()],
            tools: vec![],
            permissions: None,
            workspace: None,
            eager: None,
        };

        assert!(!platform_matches(&manifest));
    }

    #[test]
    fn resolves_relative_command_from_kit_dir() {
        let kit_dir = Path::new("heart-kit");
        let command = resolve_command(
            kit_dir,
            &["bin/server".to_string(), "--stdio".to_string()],
        );

        assert_eq!(PathBuf::from(&command[0]), kit_dir.join("bin/server"));
        assert_eq!(command[1], "--stdio");
    }

    #[cfg(windows)]
    #[test]
    fn resolves_windows_npm_shim_before_posix_script() {
        let dir = std::env::temp_dir().join(format!("portal kit {}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("codex"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("codex.cmd"), "@echo off\r\n").unwrap();
        for program in [
            "codex".to_string(),
            "./codex".to_string(),
            dir.join("codex").to_string_lossy().into_owned(),
        ] {
            let command = resolve_command(&dir, &[program, "mcp-server".into()]);
            assert_eq!(
                PathBuf::from(&command[0]).canonicalize().unwrap(),
                dir.join("codex.cmd").canonicalize().unwrap()
            );
            assert!(command_binary_exists(&command));
            assert_eq!(command[1], "mcp-server");
        }
        std::fs::remove_file(dir.join("codex.cmd")).unwrap();
        assert!(find_binary_at(&dir.join("codex")).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn manifest_validation_reports_warning_conditions() {
        let manifest = KitManifest {
            name: "".to_string(),
            version: "0.1.0".to_string(),
            description: None,
            author: None,
            platform: None,
            runtime: None,
            command: vec![],
            tools: vec![],
            permissions: None,
            workspace: None,
            eager: None,
        };

        let warnings = manifest_validation_warnings(&manifest, &[]);

        assert_eq!(warnings.len(), 3);
        assert!(warnings.iter().any(|warning| warning.contains("invalid name")));
        assert!(warnings.iter().any(|warning| warning.contains("command is empty")));
        assert!(warnings.iter().any(|warning| warning.contains("no tools defined")));
    }

    #[test]
    fn manifest_validation_warns_when_command_binary_is_missing() {
        let manifest = KitManifest {
            name: "missing-command".to_string(),
            version: "0.1.0".to_string(),
            description: None,
            author: None,
            platform: None,
            runtime: None,
            command: vec!["/definitely/missing/portal-kit-binary".to_string()],
            tools: vec![KitToolDef {
                name: "ping".to_string(),
                description: "Ping".to_string(),
                params: serde_json::json!({"type": "object"}),
            }],
            permissions: None,
            workspace: None,
            eager: None,
        };

        let warnings = manifest_validation_warnings(&manifest, &manifest.command);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("command binary not found"));
    }

    #[test]
    fn validates_kit_names() {
        assert!(is_valid_kit_name("echo-test"));
        assert!(is_valid_kit_name("kit123"));
        assert!(is_valid_kit_name("abc-123"));
        assert!(!is_valid_kit_name(""));
        assert!(!is_valid_kit_name("echo_test"));
        assert!(!is_valid_kit_name("echo test"));
        assert!(!is_valid_kit_name("écho"));
    }
}
