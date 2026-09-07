param(
    [string]$TaskName = '',
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
$nameFile = Join-Path $Root '.portal-name'
$taskNameFile = Join-Path $Root '.portal-task-name'
if ([string]::IsNullOrWhiteSpace($TaskName)) {
    if (Test-Path -LiteralPath $taskNameFile) {
        $TaskName = (Get-Content -LiteralPath $taskNameFile -Raw).Trim()
    } elseif (Test-Path -LiteralPath $nameFile) {
        $portalName = (Get-Content -LiteralPath $nameFile -Raw).Trim()
        if (-not [string]::IsNullOrWhiteSpace($portalName)) {
            $TaskName = "HeartPortal-$portalName"
        }
    }
    if ([string]::IsNullOrWhiteSpace($TaskName)) {
        throw 'TaskName was not supplied and no saved Portal task name exists.'
    }
}

Assert-PortalTaskOwnership $Root $TaskName
Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue

Stop-PortalCheckoutProcesses $Root

Write-Output "Removed scheduled task '$TaskName'."
