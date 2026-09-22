# Shared by the Windows installers; no tasks or processes are changed on import.
. (Join-Path $PSScriptRoot 'portal-lifecycle.ps1')
function Get-PortalSavedValue([string]$Root, [string]$FileName) {
    $path = Join-Path $Root $FileName
    if ($FileName -eq '.portal-connection.url') { Protect-PortalFile $path }
    if (Test-Path -LiteralPath $path) { return (Read-PortalText $path).Trim() }
    return ''
}

function Test-PortalScriptCommand([string]$CommandLine, [string]$ScriptPath) {
    if (-not $CommandLine) { return $false }
    # Match the complete argument, not another checkout with a similar prefix.
    $pattern = '(?i)(?:^|[\s"''])' + [regex]::Escape($ScriptPath) + '(?:$|[\s"''])'
    return $CommandLine -match $pattern
}

function Assert-PortalTaskOwnership([string]$Root, [string]$TaskName) {
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $task) { return }
    foreach ($action in $task.Actions) {
        foreach ($script in @('portal-supervisor-hidden.vbs', 'portal-supervisor.ps1', 'portal-supervisor-bootstrap.ps1')) {
            if (Test-PortalScriptCommand $action.Arguments (Join-Path $Root "scripts\$script")) { return }
        }
    }
    throw "Task '$TaskName' does not belong to this checkout; choose a different -TaskName."
}

function Stop-PortalCheckoutProcesses([string]$Root, [int[]]$ExcludeProcessIds = @(), [switch]$RecordedRuntimeOnly) {
    $exe = Get-PortalExecutable $Root
    $runtime = if ($RecordedRuntimeOnly) { Read-PortalJson (Join-Path $Root '.portal-runtime.json') } else { $null }
    $supervisor = Join-Path $Root 'scripts\portal-supervisor.ps1'
    $bootstrap = Join-Path $Root 'scripts\portal-supervisor-bootstrap.ps1'
    $launcher = Join-Path $Root 'scripts\portal-supervisor-hidden.vbs'
    # Stop supervisors first, then query again for Portal. Otherwise a supervisor
    # can create another child between enumerating and terminating the old child.
    foreach ($phase in @('bootstrap', 'supervisor', 'portal')) {
        $processes = @(Get-CimInstance Win32_Process | Where-Object {
            if ($_.ProcessId -eq $PID -or $_.ProcessId -in $ExcludeProcessIds) { return $false }
            if ($phase -eq 'portal') {
                # Start clients also execute this exe. Only the published child
                # belongs to a restart; concurrent first-launch clients survive.
                if ($RecordedRuntimeOnly -and (-not $runtime -or $_.ProcessId -ne $runtime.pid)) { return $false }
                return $_.ExecutablePath -and $_.ExecutablePath.Equals($exe, [StringComparison]::OrdinalIgnoreCase) -and
                    $_.CommandLine -notmatch '(?i)(?:^|\s)"?(?:upgrade|--upgrade|--version|stop|status)"?(?:\s|$)'
            }
            if ($phase -eq 'supervisor') {
                return ($_.Name -in @('powershell.exe', 'pwsh.exe')) -and (Test-PortalScriptCommand $_.CommandLine $supervisor)
            }
            return (($_.Name -in @('powershell.exe', 'pwsh.exe', 'portal-background-v1.exe')) -and
                    (Test-PortalScriptCommand $_.CommandLine $bootstrap)) -or
                (($_.Name -in @('wscript.exe', 'cscript.exe')) -and
                    (Test-PortalScriptCommand $_.CommandLine $launcher))
        })
        foreach ($item in $processes) {
            $process = Get-Process -Id $item.ProcessId -ErrorAction SilentlyContinue
            if (-not $process) { continue }
            try {
                if ($phase -eq 'portal' -and $RecordedRuntimeOnly -and $runtime.started -and
                    $process.StartTime.ToUniversalTime().Ticks -ne $runtime.started) { continue }
                $process.Kill()
                if (-not $process.WaitForExit(5000)) { throw "Process $($item.ProcessId) did not exit." }
            } catch {
                if (-not $process.HasExited) { throw }
            } finally {
                $process.Dispose()
            }
        }
    }
}
