# Embedded in the single exe. Normal launch, status and stop all use this entry.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
$request = Read-PortalJson (Join-Path $PSScriptRoot 'request.json')
$root = (Resolve-Path -LiteralPath $request.root).Path
$owner = $null
$gate = $null
$startLock = $null
$started = $false
$createdTask = $false
$taskName = ''

try {
    if ($request.action -eq 'status') {
        $runtime = Read-PortalJson (Join-Path $root '.portal-runtime.json')
        [pscustomobject]@{ ready = (Test-PortalReady $root); supervised = (Test-SavedSupervisor $runtime); pid = $runtime.pid; root = $root } | ConvertTo-Json -Compress | Write-Output
        exit 0
    }
    if ($request.action -eq 'start') { Write-Output 'PORTAL_PROGRESS:lock' }
    $startLock = Open-PortalLock $root '.portal-start.lock' 65
    if (-not $startLock) { throw 'Another Portal start/stop is still running.' }
    $owner = Open-PortalLock $root '.portal-upgrade.lock'
    if (-not $owner) { throw 'Portal upgrade is in progress; retry after it completes.' }
    $gate = Open-PortalLock $root '.portal-lifecycle.lock' 15
    if (-not $gate) { throw 'Portal lifecycle is busy.' }
    if (Test-Path -LiteralPath (Join-Path $root '.portal-upgrade.json')) { throw 'An interrupted upgrade requires recovery before starting Portal.' }
    $taskName = Get-PortalSavedValue $root '.portal-task-name'
    if ($taskName) { Assert-PortalTaskOwnership $root $taskName }

    if ($request.action -eq 'stop') {
        if ($taskName -and (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue)) {
            Disable-ScheduledTask -TaskName $taskName | Out-Null
            Stop-ScheduledTask -TaskName $taskName -ErrorAction Stop
        }
        Stop-PortalCheckoutProcesses $root @([int]$request.parent_pid) -RecordedRuntimeOnly
        Write-Output 'Portal and its supervisors stopped. Run the exe again to resume.'
        exit 0
    }

    # Repair existing registrations without restarting a live supervisor. The
    # immutable helper stays separate so upgrades can replace the engine.
    $background = Join-Path $root 'scripts\portal-background-v1.exe'
    if (-not (Test-Path -LiteralPath $background)) {
        [IO.Directory]::CreateDirectory((Join-Path $root 'scripts')) | Out-Null
        [IO.File]::Copy((Join-Path $PSScriptRoot 'support\portal-background-v1.exe'), $background)
    }
    if ($taskName) {
        $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        if ($task -and $task.Actions[0].Execute -ne $background) {
            $arguments = '-File ' + (ConvertTo-PortalArgument (Join-Path $root 'scripts\portal-supervisor-bootstrap.ps1')) + ' -Root ' + (ConvertTo-PortalArgument $root)
            $action = New-ScheduledTaskAction -Execute $background -Argument $arguments -WorkingDirectory $root
            Set-ScheduledTask -TaskName $taskName -Action $action | Out-Null
        }
    }

    $runtime = Read-PortalJson (Join-Path $root '.portal-runtime.json')
    $running = Get-PortalRecordedProcess $root $runtime
    $hasRuntime = $null -ne $running
    if ($running) { $running.Dispose() }
    if ((Test-SavedSupervisor $runtime) -or $hasRuntime) {
        if ($request.explicit) {
            $saved = Read-PortalJson (Join-Path $root '.portal-launch.json')
            if ($saved -and (($saved.arguments | ConvertTo-Json -Compress) -ne ($request.launch.arguments | ConvertTo-Json -Compress) -or
                $saved.environment.PORTAL_CONNECT_LINK -ne $request.launch.environment.PORTAL_CONNECT_LINK)) {
                throw 'Portal is already running with different settings. Run the exe with stop before changing its launch arguments.'
            }
        }
        Write-Output 'PORTAL_PROGRESS:reuse'
        if (-not (Test-SavedSupervisor $runtime)) {
            # Leave the live runtime and its launch settings intact. The new
            # core takes ownership under this same gate after we release it.
            Ensure-PortalBootstrap $root $runtime (Join-Path $PSScriptRoot 'support')
        }
        $gate.Dispose(); $gate = $null
        Wait-PortalReady $root ''
        $timer = [Diagnostics.Stopwatch]::StartNew()
        while (-not (Test-SavedSupervisor (Read-PortalJson (Join-Path $root '.portal-runtime.json')))) {
            if ($timer.Elapsed.TotalSeconds -ge 30) { throw 'Portal is running, but supervisor recovery timed out; the existing Portal was left running.' }
            Start-Sleep -Milliseconds 200
        }
        Write-Output 'Portal is already running under supervision.'
        exit 0
    }

    Write-Output 'PORTAL_PROGRESS:config'
    $relative = $request.target.Substring($root.TrimEnd('\').Length + 1)
    if ($relative -notin @('heart-portal.exe', 'heart-portal-windows-x86_64.exe', 'heart-portal-windows-aarch64.exe', 'target\release\heart-portal.exe')) { throw 'Invalid Portal executable path.' }
    $savedExe = Get-PortalSavedValue $root '.portal-executable'
    if ($savedExe -and $savedExe -ne $relative) { throw "This folder already manages $savedExe; run that executable." }
    [IO.File]::WriteAllText((Join-Path $root '.portal-executable'), $relative)
    if ($taskName -and (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue)) { Stop-ScheduledTask -TaskName $taskName }
    Stop-PortalCheckoutProcesses $root @([int]$request.parent_pid) -RecordedRuntimeOnly
    $launch = $request.launch
    if (-not $launch) { $launch = Read-PortalJson (Join-Path $root '.portal-launch.json') }
    if (-not $launch -or $launch.protocol -ne 1) { throw 'Portal launch configuration is missing.' }
    $config = [string]$launch.arguments[1]
    if (-not (Test-Path -LiteralPath $config)) {
        if (-not $request.initialize_config) { throw "Config not found: $config" }
        & $request.target config init | Out-Null
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $config -PathType Leaf)) {
            throw 'Cannot initialize the user Portal configuration.'
        }
    }
    $scripts = Join-Path $root 'scripts'
    [IO.Directory]::CreateDirectory($scripts) | Out-Null
    $firstBootstrap = -not (Test-Path -LiteralPath (Join-Path $root '.portal-launch.json'))
    foreach ($name in @('portal-lifecycle.ps1','portal-supervisor.ps1','portal-supervisor-bootstrap.ps1','portal-supervisor-hidden.vbs')) {
        $destination = Join-Path $scripts $name
        $source = Join-Path $PSScriptRoot "support\$name"
        # A normal start can repair an interrupted extraction, but upgrades own
        # replacement of existing versioned files and their rollback history.
        if (-not (Test-Path -LiteralPath $destination)) { [IO.File]::Copy($source, $destination) }
        elseif ($firstBootstrap -and $name -ne 'portal-supervisor-bootstrap.ps1') {
            $temp = "$destination.start.tmp"
            [IO.File]::Copy($source, $temp, $true)
            [IO.File]::Replace($temp, $destination, [NullString]::Value)
        }
    }
    Write-PortalJson (Join-Path $root '.portal-launch.json') $launch
    [IO.File]::WriteAllText((Join-Path $root '.portal-name'), [string]$launch.name)

    if (-not $taskName) {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $hash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($root.ToLowerInvariant()))).Replace('-', '').Substring(0,16) }
        finally { $sha.Dispose() }
        $taskName = "HeartPortal-$hash"
    }
    Assert-PortalTaskOwnership $root $taskName
    $autostart = $true
    $warning = ''
    Write-Output 'PORTAL_PROGRESS:task'
    try {
        $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
        if (-not $task) {
            $arguments = '-File ' +
                (ConvertTo-PortalArgument (Join-Path $scripts 'portal-supervisor-bootstrap.ps1')) + ' -Root ' + (ConvertTo-PortalArgument $root)
            $action = New-ScheduledTaskAction -Execute $background -Argument $arguments -WorkingDirectory $root
            $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
            $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
            $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
            $settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
            Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description 'Heart Portal automatic supervision and upgrade recovery.' -ErrorAction Stop | Out-Null
            $createdTask = $true
        } else { Enable-ScheduledTask -TaskName $taskName | Out-Null }
        [IO.File]::WriteAllText((Join-Path $root '.portal-task-name'), $taskName)
        Start-ScheduledTask -TaskName $taskName -ErrorAction Stop
    } catch {
        # A corporate scheduler policy must not silently remove crash recovery
        # for the current session. Still start supervision and report the limit.
        $autostart = $false
        $warning = "Windows logon task unavailable: $($_.Exception.Message)"
        Ensure-PortalBootstrap $root $null (Join-Path $PSScriptRoot 'support')
    }
    $started = $true
    $gate.Dispose(); $gate = $null
    Write-Output 'PORTAL_PROGRESS:ready'
    Wait-PortalReady $root $request.version
    $runtime = Read-PortalJson (Join-Path $root '.portal-runtime.json')
    if (-not (Test-SavedSupervisor $runtime)) { throw 'Portal started without a live supervisor.' }
    Write-PortalJson (Join-Path $root '.portal-start-status.json') @{ state = 'running'; pid = $runtime.pid; autostart = $autostart; warning = $warning }
    Write-Output "Portal is running under supervision (PID $($runtime.pid)). Logs: $root\portal-runtime.log"
    if ($warning) { Write-Output $warning }
} catch {
    $failure = $_.Exception.Message
    if ($started) {
        if (-not $gate) { $gate = Open-PortalLock $root '.portal-lifecycle.lock' 15 }
        if ($gate) {
            if ($taskName -and (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue)) { Stop-ScheduledTask -TaskName $taskName }
            Stop-PortalCheckoutProcesses $root @([int]$request.parent_pid) -RecordedRuntimeOnly
        }
    }
    if ($createdTask) { Unregister-ScheduledTask -TaskName $taskName -Confirm:$false }
    Write-PortalJson (Join-Path $root '.portal-start-status.json') @{ state = 'failed'; message = $failure }
    [Console]::Error.WriteLine($failure)
    exit 1
} finally {
    if ($gate) { $gate.Dispose() }
    if ($owner) { $owner.Dispose() }
    if ($startLock) { $startLock.Dispose() }
}
