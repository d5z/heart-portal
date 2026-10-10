use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use tracing::{debug, info, warn};

use crate::config::PortalConfig;

use super::auth::AuthState;
use super::environment::KitEnvironment;
use super::manifest::KitManifest;

#[derive(Debug, Clone)]
pub struct LoadedKit {
    pub manifest: KitManifest,
    pub kit_dir: PathBuf,
    pub command: Vec<String>,
    pub environment: KitEnvironment,
    pub auth: AuthState,
}

impl LoadedKit {
    pub fn configuration_error(&self) -> Option<&str> {
        self.environment
            .error
            .as_deref()
            .or(self.auth.error.as_deref())
    }
}

pub struct KitScan {
    pub kits: Vec<LoadedKit>,
    /// Retain the last valid kit while an installer is writing its manifest.
    pub invalid_dirs: Vec<PathBuf>,
}

pub fn load_kits(config: &PortalConfig) -> Result<Vec<LoadedKit>> {
    if !config.kits_enabled {
        debug!("Kits disabled in configuration");
        return Ok(Vec::new());
    }

    load_kits_from_dir(&kits_dir(config))
}

pub fn load_kits_from_dir(kits_dir: &Path) -> Result<Vec<LoadedKit>> {
    Ok(scan_kits_from_dir(kits_dir)?.kits)
}

pub fn scan_kits_from_dir(kits_dir: &Path) -> Result<KitScan> {
    let mut diagnostics = HashSet::new();
    if !kits_dir.try_exists()? {
        report_diagnostics(kits_dir, diagnostics);
        debug!("No kits directory at {}", kits_dir.display());
        return Ok(KitScan {
            kits: vec![],
            invalid_dirs: vec![],
        });
    }

    let mut entries = std::fs::read_dir(kits_dir)
        .with_context(|| format!("Reading kits directory {}", kits_dir.display()))?
        .take(4097)
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("Reading entries from kits directory {}", kits_dir.display()))?;
    entries.sort_by_key(|entry| entry.path());
    anyhow::ensure!(
        entries.len() <= 4096,
        "Too many entries in kits directory; keeping current inventory"
    );

    let mut kits = Vec::new();
    let mut invalid_dirs = Vec::new();
    let mut manifests = 0;
    for entry in entries {
        let kit_dir = entry.path();
        if is_ignored_kit_dir(&kit_dir) {
            debug!(
                "Skipping ignored kit backup directory {}",
                kit_dir.display()
            );
            continue;
        }
        match std::fs::metadata(&kit_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(err) => {
                // A temporary access failure must not be treated as uninstall.
                diagnostics.insert(format!(
                    "Cannot inspect kit directory {}: {}",
                    kit_dir.display(),
                    err
                ));
                invalid_dirs.push(kit_dir);
                continue;
            }
        }

        let manifest_path = kit_dir.join("manifest.json");
        if !manifest_path.exists() {
            invalid_dirs.push(kit_dir);
            continue;
        }

        manifests += 1;
        anyhow::ensure!(
            manifests <= 128,
            "Too many kit manifests; keeping current inventory"
        );

        match load_manifest(&kit_dir, &manifest_path, &mut diagnostics) {
            Ok(Some(kit)) => kits.push(kit),
            Ok(None) => {}
            Err(err) => {
                invalid_dirs.push(kit_dir);
                diagnostics.insert(format!(
                    "Skipping kit manifest {}: {}",
                    manifest_path.display(),
                    err
                ));
            }
        }
    }

    // Never let another directory take over an existing kit name or tool route.
    // Keeping both directories invalid preserves the previously loaded owner.
    let mut owners = std::collections::HashMap::new();
    let mut conflicts = std::collections::HashSet::new();
    let mut conflict_pairs = std::collections::HashSet::new();
    for (index, kit) in kits.iter().enumerate() {
        let routes = std::iter::once(format!("kit:{}", kit.manifest.name)).chain(
            kit.manifest
                .tools
                .iter()
                .map(|tool| format!("tool:{}", tool_route(&kit.manifest.name, &tool.name))),
        );
        for route in routes {
            if let Some(previous) = owners.insert(route.clone(), index) {
                conflicts.insert(previous);
                conflicts.insert(index);
                let pair = if previous < index {
                    (previous, index)
                } else {
                    (index, previous)
                };
                if conflict_pairs.insert(pair) {
                    diagnostics.insert(format!(
                        "Kit conflict for route '{}': '{}' and '{}'; both directories excluded",
                        route,
                        kits[previous].kit_dir.display(),
                        kit.kit_dir.display()
                    ));
                }
            }
        }
    }
    let kits = kits
        .into_iter()
        .enumerate()
        .filter_map(|(index, kit)| {
            if conflicts.contains(&index) {
                invalid_dirs.push(kit.kit_dir);
                None
            } else {
                Some(kit)
            }
        })
        .collect();

    report_diagnostics(kits_dir, diagnostics);
    Ok(KitScan { kits, invalid_dirs })
}

fn is_ignored_kit_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name == "kit-retired"
        || name.starts_with("kit-retired-")
        || name.ends_with("-kit-retired")
        || name.ends_with(".kit-retired")
}

fn load_manifest(
    kit_dir: &Path,
    manifest_path: &Path,
    diagnostics: &mut HashSet<String>,
) -> Result<Option<LoadedKit>> {
    let content = crate::bounded_file::text(manifest_path, 256 * 1024)
        .with_context(|| format!("Reading kit manifest {}", manifest_path.display()))?;
    let had_bom = content.starts_with('\u{feff}');
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let manifest: KitManifest = serde_json::from_str(content).with_context(|| {
        let bom_note = if had_bom {
            " manifest.json started with a UTF-8 BOM; check header bytes EF BB BF."
        } else {
            " manifest.json may contain a UTF-8 BOM; check header bytes EF BB BF."
        };
        format!(
            "Parsing kit manifest {}.{}",
            manifest_path.display(),
            bom_note
        )
    })?;
    anyhow::ensure!(is_valid_kit_name(&manifest.name), "Invalid kit name");
    anyhow::ensure!(
        manifest.name.len() <= 64 && manifest.tools.len() <= 128 && manifest.command.len() <= 64,
        "Kit manifest exceeds name, tool or command limits"
    );
    anyhow::ensure!(
        manifest.tools.iter().all(|tool| !tool.name.is_empty()
            && tool.name.len() <= 128
            && tool
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.')),
        "Invalid kit tool name"
    );
    if let Some(provision) = &manifest.provision {
        anyhow::ensure!(
            provision.env.len() <= 256,
            "Too many kit environment requirements"
        );
        if let Some(auth) = &provision.auth {
            anyhow::ensure!(
                auth.methods.len() <= 16
                    && auth.methods.iter().map(|m| m.files.len()).sum::<usize>() <= 16
                    && auth.methods.iter().all(|m| m.env.len() <= 256),
                "Too many kit authentication requirements"
            );
        }
    }

    if !platform_matches(&manifest) {
        debug!(
            "Skipping kit '{}' because it does not support {}",
            manifest.name,
            current_platform()
        );
        return Ok(None);
    }

    let environment = KitEnvironment::load(kit_dir, &manifest);
    let command =
        resolve_command_with_environment(kit_dir, &manifest.command, &environment.process_values());
    for warning in manifest_validation_warnings(&manifest, &command) {
        diagnostics.insert(warning);
    }

    let auth = AuthState::load(kit_dir, &manifest, &environment);
    Ok(Some(LoadedKit {
        environment,
        auth,
        manifest,
        kit_dir: kit_dir.to_path_buf(),
        command,
    }))
}

fn platform_matches(manifest: &KitManifest) -> bool {
    let platforms = manifest.platform.as_ref().or_else(|| {
        manifest
            .provision
            .as_ref()
            .map(|p| &p.platforms)
            .filter(|p| !p.is_empty())
    });
    let Some(platforms) = platforms else {
        return true;
    };

    let current = current_platform();
    platforms.iter().any(|platform| {
        let normalized = platform.to_ascii_lowercase();
        normalized == current
            || (current == "darwin" && normalized == "macos")
            || (current == "windows" && normalized == "win32")
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

#[cfg(test)]
fn resolve_command(kit_dir: &Path, command: &[String]) -> Vec<String> {
    resolve_command_with_environment(
        kit_dir,
        command,
        &KitEnvironment::default().process_values(),
    )
}

fn resolve_command_with_environment(
    kit_dir: &Path,
    command: &[String],
    env: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let mut resolved = command.to_vec();
    if resolved.is_empty() {
        return resolved;
    }

    let first = Path::new(&resolved[0]);

    if first.is_absolute() {
        if let Some(program) =
            find_binary_at_with_extensions(first, env.get("PATHEXT").map(String::as_str))
        {
            resolved[0] = program.to_string_lossy().into_owned();
        }
        return resolved;
    }

    let candidate = kit_dir.join(first);
    let has_path_separator = resolved[0].contains('/') || resolved[0].contains('\\');
    if let Some(program) =
        find_binary_at_with_extensions(&candidate, env.get("PATHEXT").map(String::as_str))
    {
        resolved[0] = program.to_string_lossy().into_owned();
        return resolved;
    }
    if has_path_separator {
        resolved[0] = candidate.to_string_lossy().to_string();
        return resolved;
    }

    if let Some(path) = env.get("PATH") {
        for dir in std::env::split_paths(path) {
            // Relative PATH entries are relative to the child's working directory.
            let path = kit_dir.join(dir).join(first);
            if let Some(program) =
                find_binary_at_with_extensions(&path, env.get("PATHEXT").map(String::as_str))
            {
                resolved[0] = program.to_string_lossy().to_string();
                return resolved;
            }
        }
    }
    // Do not let preflight or CreateProcess fall back to the host's PATH when
    // the kit explicitly selected a different environment with no such binary.
    resolved[0] = candidate.to_string_lossy().into_owned();
    resolved
}

pub(super) fn tool_route(kit: &str, tool: &str) -> String {
    format!("{kit}_{tool}").replace('-', "_")
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
        // Kit loading already resolved extensions using the kit's PATHEXT.
        // Re-resolving here using the host environment could select another file.
        #[cfg(windows)]
        if path.extension().is_none() {
            return false;
        }
        return path.is_file();
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
    find_binary_at_with_extensions(path, std::env::var("PATHEXT").ok().as_deref())
}

fn find_binary_at_with_extensions(path: &Path, _pathext: Option<&str>) -> Option<PathBuf> {
    // npm installs both a POSIX shim and a `.cmd` launcher. On Windows,
    // selecting the extensionless file first fails with Win32 error 193.
    #[cfg(windows)]
    if path.extension().is_none() {
        for ext in _pathext
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
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
    crate::paths::expand_home(Path::new(path)).unwrap_or_else(|err| {
        warn!("Cannot expand kit directory: {}", err);
        PathBuf::from(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_empty_path_does_not_fall_back_to_host_runtime() {
        let root = crate::kits::tests::TestKits::new();
        #[cfg(windows)]
        let program = "cmd.exe";
        #[cfg(not(windows))]
        let program = "sh";
        let env = std::collections::HashMap::from([("PATH".into(), String::new())]);
        let command = resolve_command_with_environment(&root.0, &[program.into()], &env);
        assert_eq!(PathBuf::from(&command[0]), root.0.join(program));
        assert!(!command_binary_exists(&command));
    }

    #[cfg(windows)]
    #[test]
    fn kit_pathext_controls_shim_selection_and_does_not_fall_back() {
        let root = crate::kits::tests::TestKits::new();
        std::fs::write(root.0.join("runtime.exe"), "fixture").unwrap();
        std::fs::write(root.0.join("runtime.cmd"), "@echo off\r\n").unwrap();
        let mut env = std::collections::HashMap::from([
            ("PATH".into(), root.0.to_string_lossy().into_owned()),
            ("PATHEXT".into(), ".CMD;.EXE".into()),
        ]);
        let command = resolve_command_with_environment(&root.0, &["runtime".into()], &env);
        assert_eq!(PathBuf::from(&command[0]), root.0.join("runtime.CMD"));
        assert!(command_binary_exists(&command));
        env.insert("PATHEXT".into(), String::new());
        let command = resolve_command_with_environment(&root.0, &["runtime".into()], &env);
        assert!(!command_binary_exists(&command));
    }
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
            provision: None,
        };

        assert!(!platform_matches(&manifest));
    }

    #[test]
    fn resolves_relative_command_from_kit_dir() {
        let kit_dir = Path::new("heart-kit");
        let command = resolve_command(kit_dir, &["bin/server".to_string(), "--stdio".to_string()]);

        assert_eq!(PathBuf::from(&command[0]), kit_dir.join("bin/server"));
        assert_eq!(command[1], "--stdio");
    }

    #[cfg(windows)]
    #[test]
    fn resolves_windows_npm_shim_before_posix_script() {
        let dir = std::env::temp_dir().join(format!("portal kit {}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("kit-runner"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("kit-runner.cmd"), "@echo off\r\n").unwrap();
        for program in [
            "kit-runner".to_string(),
            "./kit-runner".to_string(),
            dir.join("kit-runner").to_string_lossy().into_owned(),
        ] {
            let command = resolve_command(&dir, &[program, "mcp-server".into()]);
            assert_eq!(
                PathBuf::from(&command[0]).canonicalize().unwrap(),
                dir.join("kit-runner.cmd").canonicalize().unwrap()
            );
            assert!(command_binary_exists(&command));
            assert_eq!(command[1], "mcp-server");
        }
        std::fs::remove_file(dir.join("kit-runner.cmd")).unwrap();
        assert!(find_binary_at(&dir.join("kit-runner")).is_none());
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
            provision: None,
        };

        let warnings = manifest_validation_warnings(&manifest, &[]);

        assert_eq!(warnings.len(), 3);
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("invalid name")));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("command is empty")));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("no tools defined")));
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
            provision: None,
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

// Keep diagnostics across scans, including explicit reloads. A successful scan
// clears resolved errors so a later recurrence is reported again.
fn report_diagnostics(root: &Path, current: HashSet<String>) {
    static PREVIOUS: OnceLock<Mutex<HashMap<PathBuf, HashSet<String>>>> = OnceLock::new();
    let mut previous = PREVIOUS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let old = previous.entry(root.to_path_buf()).or_default();
    let (added, resolved) = diagnostic_changes(old, &current);
    for message in added {
        warn!("{}", message);
    }
    for message in resolved {
        info!("Kit issue resolved: {}", message);
    }
    *old = current;
}

fn diagnostic_changes<'a>(
    old: &'a HashSet<String>,
    current: &'a HashSet<String>,
) -> (Vec<&'a String>, Vec<&'a String>) {
    (
        current.difference(old).collect(),
        old.difference(current).collect(),
    )
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn reports_changes_and_recurrence_only() {
        let empty = HashSet::new();
        let missing = HashSet::from(["missing binary".to_owned()]);
        let conflict = HashSet::from(["route conflict".to_owned()]);
        assert_eq!(diagnostic_changes(&empty, &missing).0.len(), 1);
        assert_eq!(diagnostic_changes(&missing, &missing), (vec![], vec![]));
        let (added, resolved) = diagnostic_changes(&missing, &conflict);
        assert_eq!((added.len(), resolved.len()), (1, 1));
        assert_eq!(diagnostic_changes(&missing, &empty).1.len(), 1);
        assert_eq!(diagnostic_changes(&empty, &missing).0.len(), 1);
    }
}
