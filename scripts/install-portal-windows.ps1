param(
    [Parameter(Mandatory = $true)]
    [string]$ConnectLink,
    [string]$PortalName = '',
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$TaskName = '',
    [switch]$UseRsproxy,
    [switch]$Rebuild
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
if ([string]::IsNullOrWhiteSpace($ConnectLink)) { throw 'ConnectLink cannot be empty.' }
$uri = Get-PortalLoomUri $ConnectLink
if ([string]::IsNullOrWhiteSpace($PortalName)) {
    $PortalName = Get-PortalSavedValue $Root '.portal-name'
}
if ([string]::IsNullOrWhiteSpace($PortalName)) {
    $beingName = $uri.AbsolutePath.Trim('/').Split('/')[0]
    if ([string]::IsNullOrWhiteSpace($beingName)) { throw 'Cannot derive Being name from ConnectLink.' }
    $PortalName = "$beingName-$($env:COMPUTERNAME.ToLowerInvariant())"
}
if ($PortalName -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') {
    throw 'PortalName must contain only letters, numbers, hyphens, or underscores.'
}

$exe = Join-Path $Root 'target\release\heart-portal.exe'
$config = Join-Path $Root 'portal.toml'
$exampleConfig = Join-Path $Root 'portal.example.toml'

$builtBinary = ''
if ($Rebuild -or -not (Test-Path -LiteralPath $exe)) {
    $cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
    if (-not $cargo) {
        throw "Release binary not found: $exe. Install Rust and run 'cargo build --release' first."
    }
    $cargoArgs = @()
    if ($UseRsproxy) {
        # A config file preserves TOML quotes on both PowerShell 5.1 and 7.
        $cargoArgs += @('--config', (Join-Path $Root '.cargo\rsproxy.toml'))
    }
    # Build away from the running executable; only stop it once compilation succeeds.
    $buildDir = Join-Path $Root 'target\windows-install'
    $cargoArgs += @('build', '--release', '--locked', '--target-dir', $buildDir)
    $builtBinary = Join-Path $buildDir 'release\heart-portal.exe'
    Push-Location -LiteralPath $Root
    try {
        & $cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed with exit code $LASTEXITCODE." }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path -LiteralPath $config)) {
    if (-not (Test-Path -LiteralPath $exampleConfig)) {
        throw "Portal config and example config are both missing under: $Root"
    }
    Copy-Item -LiteralPath $exampleConfig -Destination $config
    Write-Output "Created local config from portal.example.toml: $config"
}

$workspace = Join-Path $Root 'workspace'
if (-not (Test-Path -LiteralPath $workspace)) {
    New-Item -ItemType Directory -Path $workspace | Out-Null
}

$linkFile = Join-Path $Root '.portal-connection.url'

if ([string]::IsNullOrWhiteSpace($TaskName)) {
    $TaskName = Get-PortalSavedValue $Root '.portal-task-name'
}
if ([string]::IsNullOrWhiteSpace($TaskName)) {
    $TaskName = "HeartPortal-$PortalName"
}

$install = Join-Path $PSScriptRoot 'install-portal-task.ps1'
& $install -Root $Root -TaskName $TaskName -PortalName $PortalName -BinaryPath $builtBinary -ConnectLink $ConnectLink
Write-Output "Connection saved to $linkFile (this file is Git-ignored)."
