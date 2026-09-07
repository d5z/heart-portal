# Shared by the Windows installers; no tasks or processes are changed on import.
function Get-PortalSavedValue([string]$Root, [string]$FileName) {
    $path = Join-Path $Root $FileName
    if (Test-Path -LiteralPath $path) { return (Get-Content -LiteralPath $path -Raw).Trim() }
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
        foreach ($script in @('portal-supervisor-hidden.vbs', 'portal-supervisor.ps1')) {
            if (Test-PortalScriptCommand $action.Arguments (Join-Path $Root "scripts\$script")) { return }
        }
    }
    throw "Task '$TaskName' does not belong to this checkout; choose a different -TaskName."
}

function Stop-PortalCheckoutProcesses([string]$Root) {
    $exe = Join-Path $Root 'target\release\heart-portal.exe'
    $supervisor = Join-Path $Root 'scripts\portal-supervisor.ps1'
    $launcher = Join-Path $Root 'scripts\portal-supervisor-hidden.vbs'
    # Stop supervisors first, then query again for Portal. Otherwise a supervisor
    # can create another child between enumerating and terminating the old child.
    foreach ($phase in @('supervisor', 'portal')) {
        $processes = @(Get-CimInstance Win32_Process | Where-Object {
            if ($_.ProcessId -eq $PID) { return $false }
            if ($phase -eq 'portal') {
                return $_.Name -eq 'heart-portal.exe' -and $_.ExecutablePath -and
                    $_.ExecutablePath.Equals($exe, [StringComparison]::OrdinalIgnoreCase)
            }
            return (($_.Name -in @('powershell.exe', 'pwsh.exe')) -and
                    (Test-PortalScriptCommand $_.CommandLine $supervisor)) -or
                (($_.Name -in @('wscript.exe', 'cscript.exe')) -and
                    (Test-PortalScriptCommand $_.CommandLine $launcher))
        })
        foreach ($item in $processes) {
            $process = Get-Process -Id $item.ProcessId -ErrorAction SilentlyContinue
            if (-not $process) { continue }
            try {
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
