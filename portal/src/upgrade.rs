//! Self-upgrade: check GitHub releases, download, backup, replace, restart.

use std::cmp::Ordering;
#[cfg(not(any(windows, target_os = "macos")))]
use std::path::Path;
#[cfg(not(any(windows, target_os = "macos")))]
use std::path::PathBuf;
#[cfg(not(any(windows, target_os = "macos")))]
use std::process::Stdio;
#[cfg(not(any(windows, target_os = "macos")))]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
#[cfg(not(any(windows, target_os = "macos")))]
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

#[cfg(not(any(windows, target_os = "macos")))]
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

#[cfg(not(any(windows, target_os = "macos")))]
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn binary_path(install_dir: &Path) -> PathBuf {
    install_dir.join("heart-portal")
}

fn asset_name(platform: &Platform) -> String {
    let suffix = if platform.slug.starts_with("windows-") {
        ".exe"
    } else {
        ""
    };
    format!("heart-portal-{}{}", platform.slug, suffix)
}

fn release_download_url(platform: &Platform, tag: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(&format!("https://github.com/{REPO}/releases/download/"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid release URL"))?
        .pop_if_empty()
        .push(tag)
        .push(&asset_name(platform));
    Ok(url)
}

async fn fetch_latest_release(client: &reqwest::Client) -> Result<serde_json::Value> {
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

    Ok(body)
}

fn verify_release_digest(release: &serde_json::Value, platform: &Platform, bytes: &[u8]) -> Result<()> {
    let digest = release["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|asset| asset["name"].as_str() == Some(asset_name(platform).as_str())))
        .and_then(|asset| asset["digest"].as_str());
    // Windows releases have no Authenticode signature. Require the checksum
    // supplied independently by GitHub's HTTPS API before staging or executing
    // downloaded code; the worker's own hash only protects the local handoff.
    anyhow::ensure!(digest.is_some() || !platform.slug.starts_with("windows-"),
        "Windows release is missing its GitHub SHA-256 digest; the installed Portal was left unchanged");
    if let Some(digest) = digest {
        use sha2::{Digest, Sha256};
        anyhow::ensure!(
            digest.eq_ignore_ascii_case(&format!("sha256:{:x}", Sha256::digest(bytes))),
            "Release asset checksum mismatch; the installed Portal was left unchanged"
        );
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn backup_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(not(any(unix, windows)))]
    {
        let _ = install_dir;
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
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
    #[cfg(not(any(unix, windows)))]
    {
        let _ = install_dir;
        info!("Automatic restart is not supported on this platform");
        Ok(())
    }
}

pub fn ensure_standalone_upgrade() -> Result<()> {
    anyhow::ensure!(std::env::var("HEART_PORTAL_CLIENT_MANAGED").as_deref() != Ok("1"),
        "Portal is managed by Town-Client; update it through the client to stop and replace its supervisor safely");
    Ok(())
}

pub async fn run_upgrade() -> Result<()> {
    ensure_standalone_upgrade()?;
    eprintln!("Checking for updates...");
    let platform = detect_platform()?;
    #[cfg(windows)]
    crate::windows_upgrade::installation_root(&std::env::current_exe()?)?;
    #[cfg(target_os = "macos")]
    crate::macos_upgrade::installation_root(&std::env::current_exe()?)?;
    let current_version = PORTAL_VERSION.to_string();

    let client = reqwest::Client::builder()
        .user_agent("heart-portal-upgrader")
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("Failed to create HTTP client")?;

    let release = fetch_latest_release(&client).await?;
    let tag = release["tag_name"]
        .as_str()
        .context("GitHub release response missing tag_name")?;
    let latest_version = tag.trim_start_matches('v');
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
    let download_url = release_download_url(&platform, tag)?;
    let bytes = client
        .get(download_url.clone())
        .send()
        .await
        .context("Download failed — check your internet connection")?
        .error_for_status()
        .with_context(|| format!("Download failed from {}", download_url))?
        .bytes()
        .await
        .context("Failed to read downloaded binary")?;

    verify_release_digest(&release, &platform, &bytes)?;

    #[cfg(windows)]
    return crate::windows_upgrade::handoff(&bytes, latest_version).await;
    #[cfg(target_os = "macos")]
    return crate::macos_upgrade::handoff(&bytes, Some(latest_version)).await;

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let install_dir = install_dir()?;
        std::fs::create_dir_all(&install_dir)
            .with_context(|| format!("Creating install dir {}", install_dir.display()))?;
        let target = binary_path(&install_dir);

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

        // The temporary file lives beside the target, so rename is atomic and
        // preserves a fresh inode. Never fall back to copying over `target`:
        // on macOS that can leave the kernel's code-signing page cache keyed
        // to the old inode while its contents have changed.
        std::fs::rename(&temp_path, &target)
            .with_context(|| format!("Atomically replacing {}", target.display()))?;

        eprintln!("Done — upgraded to {}", latest_version);
        restart_portal(&install_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_release_requires_matching_asset_digest() {
        use serde_json::json;
        use sha2::{Digest, Sha256};
        let platform = Platform { slug: "windows-x86_64".into() };
        let name = asset_name(&platform);
        let bytes = b"release executable";
        let digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let valid = json!({"assets": [{"name": name, "digest": digest}]});
        verify_release_digest(&valid, &platform, bytes).unwrap();
        assert!(verify_release_digest(&valid, &platform, b"tampered executable").is_err());
        for invalid in [
            json!({}), json!({"assets": []}),
            json!({"assets": [{"name": name}]}),
            json!({"assets": [{"name": name, "digest": null}]}),
            json!({"assets": [{"name": name, "digest": ""}]}),
            json!({"assets": [{"name": name, "digest": "md5:1234"}]}),
            json!({"assets": [{"name": "another-platform", "digest": digest}]}),
        ] {
            assert!(verify_release_digest(&invalid, &platform, bytes).is_err(), "{invalid}");
        }
        // Preserve older macOS metadata compatibility; its worker independently
        // enforces the Developer ID requirement before replacement.
        verify_release_digest(&json!({}), &Platform { slug: "macos-arm64".into() }, bytes).unwrap();
    }

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
        assert_eq!(
            platform_slug("windows", "x86_64").unwrap(),
            "windows-x86_64"
        );
        assert_eq!(
            platform_slug("windows", "aarch64").unwrap(),
            "windows-aarch64"
        );
        assert!(platform_slug("windows", "i686").is_err());
    }

    #[test]
    fn windows_asset_name_uses_exe_suffix() {
        let platform = Platform {
            slug: "windows-x86_64".to_string(),
        };
        assert_eq!(asset_name(&platform), "heart-portal-windows-x86_64.exe");
        assert_eq!(release_download_url(&platform, "v0.8.1").unwrap().as_str(),
            "https://github.com/d5z/heart-portal/releases/download/v0.8.1/heart-portal-windows-x86_64.exe");
    }
}
