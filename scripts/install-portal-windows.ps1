param(
    [Parameter(Mandatory = $true)]
    [string]$ConnectLink,
    [string]$PortalName = '',
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$TaskName = ''
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
if ([string]::IsNullOrWhiteSpace($ConnectLink)) { throw 'ConnectLink cannot be empty.' }
$uri = [Uri]$ConnectLink
if (-not $uri.IsAbsoluteUri -or $uri.Scheme -notin @('http', 'https')) {
    throw 'ConnectLink must be an absolute HTTP or HTTPS Loom URL.'
}
$beingName = $uri.AbsolutePath.Trim('/').Split('/')[0]
if ([string]::IsNullOrWhiteSpace($beingName) -or $uri.Query -notmatch '(?:^\?|&)token=[^&]+') {
    throw 'ConnectLink must include a Being ID and a non-empty token query parameter.'
}
if ([string]::IsNullOrWhiteSpace($PortalName)) {
    $PortalName = Get-PortalSavedValue $Root '.portal-name'
}
if ([string]::IsNullOrWhiteSpace($PortalName)) {
    $PortalName = "$beingName-$($env:COMPUTERNAME.ToLowerInvariant())"
}
if ($PortalName -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') {
    throw 'PortalName must contain only letters, numbers, hyphens, or underscores.'
}

$exe = Join-Path $Root 'target\release\heart-portal.exe'
$config = Join-Path $Root 'portal.toml'
$exampleConfig = Join-Path $Root 'portal.example.toml'

if (-not (Test-Path -LiteralPath $exe)) {
    throw "Release binary not found: $exe. Run 'cargo build --release --locked' before installing."
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
& $install -Root $Root -TaskName $TaskName -PortalName $PortalName -ConnectLink ($ConnectLink.Trim())
Write-Output "Connection saved to $linkFile (this file is Git-ignored)."
