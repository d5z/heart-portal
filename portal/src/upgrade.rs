//! Self-upgrade: check GitHub releases, download, backup, replace, restart.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tracing::info;

pub const PORTAL_VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "d5z/heart-portal";
const GITHUB_API_LATEST: &str = "https://api.github.com/repos/d5z/heart-portal/releases/latest";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub slug: String,
}

pub fn detect_platform() -> Result<Platform> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let slug = platform_slug(os, arch)?;

    Ok(Platform {
        slug: slug.to_string(),
    })
}

fn platform_slug(os: &str, arch: &str) -> Result<&'static str> {
    Ok(match os {
        "macos" => match arch {
            "aarch64" => "macos-arm64",
            "x86_64" => "macos-x86_64",
            other => bail!("Unsupported Mac architecture: {}", other),
        },
        "linux" => match arch {
            "x86_64" => "linux-x86_64",
            "aarch64" => "linux-arm64",
            other => bail!("Unsupported Linux architecture: {}", other),
        },
        "windows" => match arch {
            "x86_64" => "windows-x86_64",
            "aarch64" => "windows-aarch64",
            other => bail!("Unsupported Windows architecture: {}", other),
        },
        other => bail!("Unsupported OS: {}", other),
    })
}

pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| -> Vec<u32> {
        v.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect()
    };

    let pa = parse(a);
    let pb = parse(b);
    let len = pa.len().max(pb.len());

    for i in 0..len {
        let da = pa.get(i).copied().unwrap_or(0);
        let db = pb.get(i).copied().unwrap_or(0);
        match da.cmp(&db) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

fn install_dir() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if parent.file_name().and_then(|n| n.to_str()) == Some(".heart-portal") {
                return Ok(parent.to_path_buf());
            }
        }
    }

    if let Some(home) = dirs_home() {
        return Ok(home.join(".heart-portal"));
    }

    bail!("Could not determine install directory (~/.heart-portal)")
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn binary_path(install_dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        install_dir.join("heart-portal.exe")
    }
    #[cfg(not(windows))]
    {
        install_dir.join("heart-portal")
    }
}

fn asset_name(platform: &Platform) -> String {
    let suffix = if platform.slug.starts_with("windows-") {
        ".exe"
    } else {
        ""
    };
    format!("heart-portal-{}{}", platform.slug, suffix)
}

fn latest_download_url(platform: &Platform) -> String {
    format!(
        "https://github.com/{}/releases/latest/download/{}",
        REPO,
        asset_name(platform)
    )
}

async fn fetch_latest_release_tag(client: &reqwest::Client) -> Result<String> {
    let response = client
        .get(GITHUB_API_LATEST)
        .header(reqwest::header::USER_AGENT, "heart-portal-upgrader")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("Could not reach GitHub — check your internet connection")?
        .error_for_status()
        .context("GitHub API returned an error while checking releases")?;

    let body: serde_json::Value = response
        .json()
        .await
        .context("Failed to parse GitHub release metadata")?;

    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("GitHub release response missing tag_name"))?;

    Ok(tag.trim_start_matches('v').to_string())
}

fn backup_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

fn stop_running_portal(install_dir: &Path) {
    #[cfg(unix)]
    {
        let stop_sh = install_dir.join("stop.sh");
        if stop_sh.is_file() {
            let _ = std::process::Command::new("sh")
                .arg(&stop_sh)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return;
        }

        let _ = std::process::Command::new("pkill")
            .args(["-f", "heart-portal"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        for script in ["stop.bat", "stop.cmd"] {
            let stop_script = install_dir.join(script);
            if stop_script.is_file() {
                let _ = std::process::Command::new("cmd")
                    .arg("/C")
                    .arg(&stop_script)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                return;
            }
        }

        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "heart-portal.exe"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = install_dir;
    }
}

#[cfg(target_os = "macos")]
pub fn unlock_gatekeeper(path: &Path) {
    // Remove ALL extended attributes — including com.apple.provenance
    // which macOS adds to files transferred via scp/AirDrop/download.
    // Without this, macOS sends SIGKILL (-9) on exec.
    let _ = std::process::Command::new("xattr")
        .args(["-cr", &path.to_string_lossy()])
        .status();

    // Ad-hoc re-sign: the linker signature from the build machine may be
    // invalidated by transfer.  A fresh ad-hoc signature lets Gatekeeper
    // and the hardened-runtime check pass without a Developer ID.
    let _ = std::process::Command::new("codesign")
        .args(["-s", "-", "--force", "--deep", &path.to_string_lossy()])
        .status();
}

#[cfg(not(target_os = "macos"))]
pub fn unlock_gatekeeper(_path: &Path) {}

fn restart_portal(install_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let start_sh = install_dir.join("start.sh");
        if !start_sh.is_file() {
            info!("No start.sh found — binary updated; start Portal manually when ready");
            return Ok(());
        }

        eprintln!("Restarting Portal...");
        std::process::Command::new("sh")
            .arg(&start_sh)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to restart Portal via start.sh")?;
        eprintln!("Portal restarted.");
        Ok(())
    }
    #[cfg(windows)]
    {
        let mut start_target = None;
        for script in ["start.bat", "start.cmd"] {
            let start_script = install_dir.join(script);
            if start_script.is_file() {
                start_target = Some(start_script);
                break;
            }
        }
        let start_target = start_target.unwrap_or_else(|| binary_path(install_dir));
        if !start_target.is_file() {
            info!("No Windows start script or binary found — start Portal manually when ready");
            return Ok(());
        }

        eprintln!("Restarting Portal...");
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("start")
            .arg("")
            .arg(&start_target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to restart Portal via cmd /C start")?;
        eprintln!("Portal restarted.");
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = install_dir;
        info!("Automatic restart is not supported on this platform");
        Ok(())
    }
}

pub async fn run_upgrade() -> Result<()> {
    eprintln!("Checking for updates...");
    let platform = detect_platform()?;
    let install_dir = install_dir()?;
    std::fs::create_dir_all(&install_dir)
        .with_context(|| format!("Creating install dir {}", install_dir.display()))?;

    let target = binary_path(&install_dir);
    let current_version = PORTAL_VERSION.to_string();

    let client = reqwest::Client::builder()
        .user_agent("heart-portal-upgrader")
        .build()
        .context("Failed to create HTTP client")?;

    let latest_version = fetch_latest_release_tag(&client).await?;
    eprintln!("  Current: {}", current_version);
    eprintln!("  Latest:  {}", latest_version);

    match compare_versions(&latest_version, &current_version) {
        Ordering::Greater => {}
        Ordering::Equal => {
            eprintln!("Already up to date ({})", current_version);
            return Ok(());
        }
        Ordering::Less => {
            eprintln!("Already up to date ({})", current_version);
            return Ok(());
        }
    }

    eprintln!("Downloading {}...", asset_name(&platform));
    let download_url = latest_download_url(&platform);
    let bytes = client
        .get(&download_url)
        .send()
        .await
        .context("Download failed — check your internet connection")?
        .error_for_status()
        .with_context(|| format!("Download failed from {}", download_url))?
        .bytes()
        .await
        .context("Failed to read downloaded binary")?;

    let temp_path = install_dir.join(format!("heart-portal.new.{}", backup_stamp()));
    tokio::fs::write(&temp_path, &bytes)
        .await
        .with_context(|| format!("Writing {}", temp_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&temp_path)
            .await
            .context("Reading permissions on downloaded binary")?
            .permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&temp_path, perms)
            .await
            .context("Setting executable permissions on downloaded binary")?;
    }

    eprintln!("Replacing binary...");
    stop_running_portal(&install_dir);

    if target.is_file() {
        let backup_path = install_dir.join(format!("heart-portal.bak.{}", backup_stamp()));
        std::fs::copy(&target, &backup_path).with_context(|| {
            format!(
                "Backing up {} to {}",
                target.display(),
                backup_path.display()
            )
        })?;
        eprintln!("  Backup: {}", backup_path.display());
    }

    std::fs::rename(&temp_path, &target).or_else(|rename_err| {
        std::fs::copy(&temp_path, &target)
            .with_context(|| format!("Copying upgrade into {}", target.display()))?;
        std::fs::remove_file(&temp_path).ok();
        if rename_err.kind() == std::io::ErrorKind::CrossesDevices {
            Ok(())
        } else {
            Err(rename_err).with_context(|| format!("Replacing {}", target.display()))
        }
    })?;

    unlock_gatekeeper(&target);
    eprintln!("Done — upgraded to {}", latest_version);
    restart_portal(&install_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions_orders_semver() {
        assert_eq!(compare_versions("0.5.0", "0.4.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.4.9", "0.5.0"), Ordering::Less);
        assert_eq!(compare_versions("0.5.0", "0.5.0"), Ordering::Equal);
        assert_eq!(compare_versions("v1.0.0", "0.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.10.0", "0.9.0"), Ordering::Greater);
    }

    #[test]
    fn detect_platform_returns_slug_on_supported_host() {
        let platform = detect_platform().expect("host platform should be supported in tests");
        assert!(!platform.slug.is_empty());
        assert!(
            platform.slug.starts_with("macos-")
                || platform.slug.starts_with("linux-")
                || platform.slug.starts_with("windows-")
        );
    }

    #[test]
    fn platform_slug_supports_windows() {
        assert_eq!(platform_slug("windows", "x86_64").unwrap(), "windows-x86_64");
        assert_eq!(platform_slug("windows", "aarch64").unwrap(), "windows-aarch64");
        assert!(platform_slug("windows", "i686").is_err());
    }

    #[test]
    fn windows_asset_name_uses_exe_suffix() {
        let platform = Platform {
            slug: "windows-x86_64".to_string(),
        };
        assert_eq!(asset_name(&platform), "heart-portal-windows-x86_64.exe");
        assert!(latest_download_url(&platform).ends_with("heart-portal-windows-x86_64.exe"));
    }
}
