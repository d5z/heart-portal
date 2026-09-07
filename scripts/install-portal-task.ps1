param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$TaskName = '',
    [string]$PortalName = '',
    [string]$ConnectLink = ''
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
$supervisor = Join-Path $Root 'scripts\portal-supervisor.ps1'
$hiddenLauncher = Join-Path $Root 'scripts\portal-supervisor-hidden.vbs'
if (-not (Test-Path -LiteralPath $supervisor)) { throw "Supervisor script not found: $supervisor" }
if (-not (Test-Path -LiteralPath $hiddenLauncher)) { throw "Hidden launcher not found: $hiddenLauncher" }

if ([string]::IsNullOrWhiteSpace($PortalName)) { $PortalName = Get-PortalSavedValue $Root '.portal-name' }
if ($PortalName -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') {
    throw 'Supply -PortalName on first installation (letters, numbers, hyphens or underscores).'
}
$previousTask = Get-PortalSavedValue $Root '.portal-task-name'
if ([string]::IsNullOrWhiteSpace($TaskName)) { $TaskName = $previousTask }
if ([string]::IsNullOrWhiteSpace($TaskName)) { $TaskName = "HeartPortal-$PortalName" }
if ($TaskName.IndexOfAny([char[]]'\/:*?"<>|') -ge 0) { throw 'TaskName contains invalid characters.' }
Assert-PortalTaskOwnership $Root $TaskName
if ($previousTask -and $previousTask -ne $TaskName) { Assert-PortalTaskOwnership $Root $previousTask }

$portalExe = Join-Path $Root 'target\release\heart-portal.exe'
if (-not (Test-Path -LiteralPath $portalExe)) {
    throw "Portal binary not found: $portalExe. Run 'cargo build --release --locked' first."
}
foreach ($required in @('portal.toml', '.portal-connection.url')) {
    if ($required -eq '.portal-connection.url' -and -not [string]::IsNullOrWhiteSpace($ConnectLink)) { continue }
    if (-not (Test-Path -LiteralPath (Join-Path $Root $required))) { throw "Missing $required; run install-portal-windows.ps1 first." }
}

$wscript = Join-Path $env:SystemRoot 'System32\wscript.exe'
if (-not (Test-Path -LiteralPath $wscript)) { throw "Windows Script Host not found: $wscript" }
$arguments = '"{0}" "{1}" "{2}"' -f $hiddenLauncher, $Root, $PortalName
$action = New-ScheduledTaskAction -Execute $wscript -Argument $arguments -WorkingDirectory $Root
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
if (-not [string]::IsNullOrWhiteSpace($ConnectLink)) {
    Set-Content -LiteralPath (Join-Path $Root '.portal-connection.url') -Value $ConnectLink.Trim() -NoNewline
}
Set-Content -LiteralPath (Join-Path $Root '.portal-name') -Value $PortalName -NoNewline
Set-Content -LiteralPath (Join-Path $Root '.portal-task-name') -Value $TaskName -NoNewline
Start-ScheduledTask -TaskName $TaskName
Write-Output "Installed and started scheduled task '$TaskName' for Portal '$PortalName'."
