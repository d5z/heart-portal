//! Screenshot tool — capture the local screen to a workspace file.

use crate::config::PortalConfig;
use crate::tools::file::resolve_write_path;
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::debug;

#[derive(Debug)]
enum CaptureRegion {
    Full,
    Rect {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
}

pub async fn capture(config: &PortalConfig, arguments: Value) -> Result<Value> {
    let path_str = arguments
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(default_screenshot_path);

    let region_str = arguments
        .get("region")
        .and_then(|v| v.as_str())
        .unwrap_or("full")
        .trim();
    let region = parse_region(region_str)?;

    let display_idx = match arguments.get("display") {
        Some(value) => Some(
            super::value_as_u64(value)
                .ok_or_else(|| anyhow::anyhow!("'display' must be a non-negative integer"))?,
        ),
        None => None,
    };

    let path = resolve_write_path(config, &path_str)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create {}: {}", parent.display(), e))?;
    }

    debug!(
        "portal_screenshot: path={} region={:?} display_idx={:?}",
        path.display(),
        region,
        &display_idx
    );

    run_capture(&path, &region, display_idx).await?;

    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to stat screenshot {}: {}", path.display(), e))?;
    if meta.len() == 0 {
        anyhow::bail!("Screenshot command created an empty file: {}", path.display());
    }

    let dimensions = read_png_dimensions(&path).await?;
    let dimensions_value = dimensions
        .map(|(width, height)| serde_json::json!({ "width": width, "height": height }))
        .unwrap_or(Value::Null);
    let saved_path = display_path(config, &path);
    let summary = serde_json::json!({
        "path": saved_path.clone(),
        "size_bytes": meta.len(),
        "dimensions": dimensions_value,
    });

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&summary)? }],
        "path": saved_path,
        "size_bytes": meta.len(),
        "dimensions": summary["dimensions"].clone(),
    }))
}

fn default_screenshot_path() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(".screenshots/capture-{}.png", millis)
}

fn parse_region(region: &str) -> Result<CaptureRegion> {
    if region.eq_ignore_ascii_case("full") || region.is_empty() {
        return Ok(CaptureRegion::Full);
    }
    if region.eq_ignore_ascii_case("window") {
        anyhow::bail!(
            "region 'window' is not supported by portal_screenshot yet; use 'full' or 'x,y,w,h'"
        );
    }

    let parts: Vec<&str> = region.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        anyhow::bail!("Invalid region '{}'; expected 'full' or 'x,y,w,h'", region);
    }

    let x = parts[0]
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("Invalid region x coordinate: '{}'", parts[0]))?;
    let y = parts[1]
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("Invalid region y coordinate: '{}'", parts[1]))?;
    let width = parts[2]
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("Invalid region width: '{}'", parts[2]))?;
    let height = parts[3]
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("Invalid region height: '{}'", parts[3]))?;

    if width <= 0 || height <= 0 {
        anyhow::bail!("Region width and height must be positive");
    }

    Ok(CaptureRegion::Rect {
        x,
        y,
        width,
        height,
    })
}

#[cfg(target_os = "macos")]
async fn run_capture(path: &Path, region: &CaptureRegion, display_idx: Option<u64>) -> Result<()> {
    let mut command = Command::new("screencapture");
    command.arg("-x");

    if let Some(disp) = display_idx {
        let screencapture_disp = disp
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("'display' is too large"))?;
        command.arg("-D").arg(screencapture_disp.to_string());
    }

    if let CaptureRegion::Rect {
        x,
        y,
        width,
        height,
    } = region
    {
        command
            .arg("-R")
            .arg(format!("{},{},{},{}", x, y, width, height));
    }

    command.arg(path);
    run_command("screencapture", command).await.map_err(|e| {
        if e.to_string().contains("not found") {
            anyhow::anyhow!("screencapture not found; portal_screenshot requires macOS screencapture")
        } else {
            e
        }
    })
}

#[cfg(target_os = "linux")]
async fn run_capture(path: &Path, region: &CaptureRegion, display_idx: Option<u64>) -> Result<()> {
    if display_idx.is_some() {
        anyhow::bail!(
            "display selection is not supported by portal_screenshot on Linux; omit 'display'"
        );
    }

    let mut errors = Vec::new();

    if let Err(e) = run_linux_import(path, region).await {
        errors.push(format!("import: {}", e));
    } else {
        return Ok(());
    }

    if matches!(region, CaptureRegion::Full) {
        if let Err(e) = run_linux_gnome_screenshot(path).await {
            errors.push(format!("gnome-screenshot: {}", e));
        } else {
            return Ok(());
        }
    }

    if let Err(e) = run_linux_scrot(path, region).await {
        errors.push(format!("scrot: {}", e));
    } else {
        return Ok(());
    }

    anyhow::bail!(
        "No screenshot utility succeeded. Install ImageMagick 'import', gnome-screenshot, or scrot. Attempts: {}",
        errors.join("; ")
    );
}

#[cfg(target_os = "linux")]
async fn run_linux_import(path: &Path, region: &CaptureRegion) -> Result<()> {
    let mut command = Command::new("import");
    command.arg("-window").arg("root");
    if let CaptureRegion::Rect {
        x,
        y,
        width,
        height,
    } = region
    {
        command
            .arg("-crop")
            .arg(format!("{}x{}+{}+{}", width, height, x, y));
    }
    command.arg(path);
    run_command("import", command).await
}

#[cfg(target_os = "linux")]
async fn run_linux_gnome_screenshot(path: &Path) -> Result<()> {
    let mut command = Command::new("gnome-screenshot");
    command.arg("-f").arg(path);
    run_command("gnome-screenshot", command).await
}

#[cfg(target_os = "linux")]
async fn run_linux_scrot(path: &Path, region: &CaptureRegion) -> Result<()> {
    let mut command = Command::new("scrot");
    if let CaptureRegion::Rect {
        x,
        y,
        width,
        height,
    } = region
    {
        command
            .arg("-a")
            .arg(format!("{},{},{},{}", x, y, width, height));
    }
    command.arg(path);
    run_command("scrot", command).await
}

#[cfg(target_os = "windows")]
async fn run_capture(path: &Path, region: &CaptureRegion, display_idx: Option<u64>) -> Result<()> {
    if display_idx.is_some() {
        anyhow::bail!(
            "display selection is not supported by portal_screenshot on Windows; omit 'display'"
        );
    }

    let script = powershell_capture_script(path, region);
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    run_command("powershell", command).await.map_err(|e| {
        if e.to_string().contains("not found") {
            anyhow::anyhow!("powershell not found; portal_screenshot on Windows requires PowerShell")
        } else {
            e
        }
    })
}

fn powershell_capture_script(path: &Path, region: &CaptureRegion) -> String {
    let path = powershell_single_quoted(&path.to_string_lossy());
    match region {
        CaptureRegion::Full => format!(
            "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
             $screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds; \
             $bitmap = New-Object System.Drawing.Bitmap($screen.Width, $screen.Height); \
             $graphics = [System.Drawing.Graphics]::FromImage($bitmap); \
             $graphics.CopyFromScreen($screen.Location, [System.Drawing.Point]::Empty, $screen.Size); \
             $bitmap.Save({}, [System.Drawing.Imaging.ImageFormat]::Png); \
             $graphics.Dispose(); $bitmap.Dispose()",
            path
        ),
        CaptureRegion::Rect {
            x,
            y,
            width,
            height,
        } => format!(
            "Add-Type -AssemblyName System.Drawing; \
             $bitmap = New-Object System.Drawing.Bitmap({w}, {h}); \
             $graphics = [System.Drawing.Graphics]::FromImage($bitmap); \
             $graphics.CopyFromScreen({x}, {y}, 0, 0, (New-Object System.Drawing.Size({w}, {h}))); \
             $bitmap.Save({path}, [System.Drawing.Imaging.ImageFormat]::Png); \
             $graphics.Dispose(); $bitmap.Dispose()",
            w = width,
            h = height,
            x = x,
            y = y,
            path = path
        ),
    }
}

fn powershell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
async fn run_capture(
    _path: &Path,
    _region: &CaptureRegion,
    _display_idx: Option<u64>,
) -> Result<()> {
    anyhow::bail!("portal_screenshot is only supported on macOS, Linux, and Windows");
}

async fn run_command(name: &str, mut command: Command) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{} timed out after 30s", name))?
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("{} not found", name)
        } else {
            anyhow::anyhow!("Failed to run {}: {}", name, e)
        }
    })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "{} failed with exit {}: {}",
        name,
        output.status.code().unwrap_or(-1),
        stderr.trim()
    );
}

async fn read_png_dimensions(path: &Path) -> Result<Option<(u32, u32)>> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to read screenshot metadata {}: {}",
                path.display(),
                e
            )
        })?;
    let mut bytes = [0u8; 24];
    let read = file
        .read(&mut bytes)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to read screenshot metadata {}: {}",
                path.display(),
                e
            )
        })?;
    Ok(parse_png_dimensions(&bytes[..read]))
}

fn parse_png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 {
        return None;
    }
    if &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }

    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

fn display_path(config: &PortalConfig, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(&config.security.workspace_root) {
        return pathbuf_to_string(rel);
    }

    if let Ok(root) = config.security.workspace_root.canonicalize() {
        if let Ok(rel) = path.strip_prefix(root) {
            return pathbuf_to_string(rel);
        }
    }

    path.display().to_string()
}

fn pathbuf_to_string(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_string()
    } else {
        path.to_string_lossy().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_region() {
        assert!(matches!(parse_region("full").unwrap(), CaptureRegion::Full));
    }

    #[test]
    fn parses_rect_region() {
        let region = parse_region("10,20,300,400").unwrap();
        assert!(matches!(
            region,
            CaptureRegion::Rect {
                x: 10,
                y: 20,
                width: 300,
                height: 400
            }
        ));
    }

    #[test]
    fn rejects_window_region() {
        assert!(parse_region("window").is_err());
    }

    #[test]
    fn parses_png_dimensions() {
        let mut bytes = vec![0; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&640u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&480u32.to_be_bytes());
        assert_eq!(parse_png_dimensions(&bytes), Some((640, 480)));
    }

    #[test]
    fn powershell_quotes_paths() {
        assert_eq!(
            powershell_single_quoted(r"C:\Users\Being's\shot.png"),
            r"'C:\Users\Being''s\shot.png'"
        );
    }

    #[test]
    fn windows_rect_script_contains_region_and_png_save() {
        let script = powershell_capture_script(
            Path::new(r"C:\tmp\shot.png"),
            &CaptureRegion::Rect {
                x: 10,
                y: 20,
                width: 300,
                height: 400,
            },
        );
        assert!(script.contains("CopyFromScreen(10, 20, 0, 0"));
        assert!(script.contains("System.Drawing.Size(300, 400)"));
        assert!(script.contains("[System.Drawing.Imaging.ImageFormat]::Png"));
    }
}
