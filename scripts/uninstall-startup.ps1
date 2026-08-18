# uninstall-startup.ps1
# Removes the HeartPortal logon task registered by install-startup.ps1.

$ErrorActionPreference = 'Stop'

$task = 'HeartPortal'

if (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $task -Confirm:$false
    Write-Host "OK: '$task' task removed."
} else {
    Write-Host "'$task' task not found - nothing to remove."
}
