# install-startup.ps1
# Registers a scheduled task that starts heart-portal when the current user logs on.
# Runs in the current user's context - no hardcoded username.
# A logon-trigger task for the current user does not require admin rights.

$ErrorActionPreference = 'Stop'

$task = 'HeartPortal'

# Known install locations (first match wins)
$candidates = @(
    (Join-Path $env:USERPROFILE 'heart-portal\heart-portal.exe'),
    (Join-Path $env:USERPROFILE '.heart-portal\heart-portal.exe')
)
$exe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $exe) {
    Write-Error "heart-portal.exe not found in any known location ($($candidates -join ', ')). Install heart-portal first."
    exit 1
}

$action   = New-ScheduledTaskAction -Execute $exe
$trigger  = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName $task -Action $action -Trigger $trigger -Settings $settings -Description 'Start heart-portal at user logon' -Force | Out-Null

Write-Host "OK: '$task' registered. heart-portal ($exe) will start when $env:USERNAME logs in."
