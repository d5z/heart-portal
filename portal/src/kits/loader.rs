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

    if manifest.command.is_empty() {
        warn!("Skipping kit '{}' because command is empty", manifest.name);
        return Ok(None);
    }

    let command = resolve_command(kit_dir, &manifest.command);

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
    let first = Path::new(&resolved[0]);

    if first.is_absolute() {
        return resolved;
    }

    let candidate = kit_dir.join(first);
    let has_path_separator = resolved[0].contains('/') || resolved[0].contains('\\');
    if has_path_separator || candidate.exists() {
        resolved[0] = candidate.to_string_lossy().to_string();
    }

    resolved
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };

        assert!(!platform_matches(&manifest));
    }

    #[test]
    fn resolves_relative_command_from_kit_dir() {
        let command = resolve_command(
            Path::new("/tmp/heart-kit"),
            &["bin/server".to_string(), "--stdio".to_string()],
        );

        assert_eq!(command[0], "/tmp/heart-kit/bin/server");
        assert_eq!(command[1], "--stdio");
    }
}
