param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$TaskName = '',
    [string]$PortalName = '',
    [string]$ConnectLink = ''
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
$sourceExe = Get-PortalExecutable $Root
$previousEncoding = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
    $installed = & $sourceExe --install-user-runtime
    if ($LASTEXITCODE -ne 0) { throw 'Cannot prepare the user Portal installation; stop the legacy Portal before migrating.' }
} finally { [Console]::OutputEncoding = $previousEncoding }
$Root = [string](($installed | ConvertFrom-Json).root)
$portalExe = Get-PortalExecutable $Root
if (-not (Test-Path -LiteralPath (Join-Path $Root 'scripts\portal-lifecycle.ps1'))) {
    & $portalExe --export-windows-runtime (Join-Path $Root 'scripts') | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Cannot prepare Portal supervision.' }
}
$maintenance = Enter-PortalMaintenance $Root
try {
$supervisor = Join-Path $Root 'scripts\portal-supervisor.ps1'
$hiddenLauncher = Join-Path $Root 'scripts\portal-background-v1.exe'
if (-not (Test-Path -LiteralPath $hiddenLauncher)) {
    & $portalExe --export-windows-launcher $hiddenLauncher | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Cannot prepare the native background launcher.' }
}
if (-not (Test-Path -LiteralPath $supervisor)) { throw "Supervisor script not found: $supervisor" }
if (-not (Test-Path -LiteralPath $hiddenLauncher)) { throw "Hidden launcher not found: $hiddenLauncher" }
if (-not (Test-Path -LiteralPath (Join-Path $Root 'scripts\portal-supervisor-bootstrap.ps1'))) { throw 'Portal supervisor bootstrap is missing.' }

if ([string]::IsNullOrWhiteSpace($PortalName)) { $PortalName = Get-PortalSavedValue $Root '.portal-name' }
if ($PortalName -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') {
    throw 'Supply -PortalName on first installation (letters, numbers, hyphens or underscores).'
}
$previousTask = Get-PortalSavedValue $Root '.portal-task-name'
if ([string]::IsNullOrWhiteSpace($TaskName)) { $TaskName = $previousTask }
if ([string]::IsNullOrWhiteSpace($TaskName)) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { $hash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Root.ToLowerInvariant()))).Replace('-', '').Substring(0,16) }
    finally { $sha.Dispose() }
    $TaskName = "HeartPortal-$hash"
}
if ($TaskName.IndexOfAny([char[]]'\/:*?"<>|') -ge 0) { throw 'TaskName contains invalid characters.' }
Assert-PortalTaskOwnership $Root $TaskName
if ($previousTask -and $previousTask -ne $TaskName) { Assert-PortalTaskOwnership $Root $previousTask }

$portalExe = Get-PortalExecutable $Root
if (-not (Test-Path -LiteralPath $portalExe)) {
    throw "Portal binary not found: $portalExe. Run 'cargo build --release --locked' first."
}
if ([string]::IsNullOrWhiteSpace($ConnectLink) -and -not (Test-Path -LiteralPath (Join-Path $Root '.portal-connection.url'))) {
    throw 'Missing .portal-connection.url; run install-portal-windows.ps1 first.'
}

# Resolve/create configuration through the same user-directory policy as the CLI.
& $portalExe config init | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot initialize the user Portal configuration.' }

$arguments = '-File "{0}" -Root "{1}" -PortalName "{2}"' -f (Join-Path $Root 'scripts\portal-supervisor-bootstrap.ps1'), $Root, $PortalName
$action = New-ScheduledTaskAction -Execute $hiddenLauncher -Argument $arguments -WorkingDirectory $Root
$currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $currentUser
$principal = New-ScheduledTaskPrincipal -UserId $currentUser -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)

# Registration can fail (permissions/policy). Leave the existing process and
# saved identity untouched until the new task definition has been accepted.
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Description 'Keeps the Heart Portal relay connection alive and restarts it after crashes.' -Force | Out-Null
Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($previousTask -and $previousTask -ne $TaskName) {
    Stop-ScheduledTask -TaskName $previousTask -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $previousTask -Confirm:$false
}
Stop-PortalCheckoutProcesses $Root
if (Test-Path -LiteralPath (Join-Path $Root '.portal-upgrade.json')) {
    Restore-PortalUpgrade $Root (Read-PortalJson (Join-Path $Root '.portal-upgrade.json'))
}
if (-not [string]::IsNullOrWhiteSpace($ConnectLink)) {
    Write-PortalPrivateText (Join-Path $Root '.portal-connection.url') $ConnectLink.Trim()
}
Set-Content -LiteralPath (Join-Path $Root '.portal-name') -Value $PortalName -NoNewline
Set-Content -LiteralPath (Join-Path $Root '.portal-task-name') -Value $TaskName -NoNewline
$relativeExe = if ($portalExe -eq (Join-Path $Root 'heart-portal.exe')) { 'heart-portal.exe' } else { 'target\release\heart-portal.exe' }
Set-Content -LiteralPath (Join-Path $Root '.portal-executable') -Value $relativeExe -NoNewline
Start-ScheduledTask -TaskName $TaskName
Write-Output "Installed and started scheduled task '$TaskName' for Portal '$PortalName'."
} finally { foreach ($lock in $maintenance) { $lock.Dispose() } }
