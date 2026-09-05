# Run in Windows PowerShell 5.1. No Pester, relay, or real scheduled task needed.
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$tempBase = [IO.Path]::GetTempPath()
$testRoot = Join-Path $tempBase ("portal Windows test " + [char]0x6D4B + '-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path (Join-Path $testRoot 'target\release') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $testRoot 'scripts') | Out-Null
Copy-Item -LiteralPath (Join-Path $repo 'portal.example.toml') -Destination $testRoot
foreach ($script in @('portal-supervisor.ps1', 'portal-supervisor-hidden.vbs', 'portal-task-common.ps1', 'install-portal-task.ps1', 'install-portal-windows.ps1', 'uninstall-portal-task.ps1')) {
    Copy-Item -LiteralPath (Join-Path $repo "scripts\$script") -Destination (Join-Path $testRoot 'scripts')
}

function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Assertion failed: $Message" }
}
function Wait-Until([scriptblock]$Condition, [string]$Message, [int]$TimeoutSeconds = 20) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 100
    } while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds)
    throw "Timed out: $Message (test files: $testRoot)"
}
function Launch-Supervisor {
    $supervisor = Join-Path $testRoot 'scripts\portal-supervisor.ps1'
    $arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "{0}" -Root "{1}" -RestartDelaySeconds 1' -f $supervisor, $testRoot
    Start-Process -FilePath (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe') -ArgumentList $arguments -WindowStyle Hidden -PassThru
}
function Get-Launches {
    $path = Join-Path $testRoot 'launches.txt'
    if (Test-Path -LiteralPath $path) { return @(Get-Content -LiteralPath $path) }
    return @()
}

try {
    $fixture = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'fake-portal.cs') -Raw
    Add-Type -TypeDefinition $fixture -OutputAssembly (Join-Path $testRoot 'target\release\heart-portal.exe') -OutputType ConsoleApplication

    # Mocks are confined to this scope, so real tests below can inspect only the
    # fixture's processes. No scheduler registration or system process kill runs.
    & {
        $global:PortalTestTasks = @{}
        $global:PortalTestFailRegistration = $false
        $global:PortalTestStarted = @()
        function Get-ScheduledTask { [CmdletBinding()] param($TaskName); return $global:PortalTestTasks[$TaskName] }
        function New-ScheduledTaskAction { param($Execute, $Argument, $WorkingDirectory); return [pscustomobject]@{ Execute = $Execute; Arguments = $Argument; WorkingDirectory = $WorkingDirectory } }
        function New-ScheduledTaskTrigger { param([switch]$AtLogOn, $User); return [pscustomobject]@{ User = $User } }
        function New-ScheduledTaskPrincipal { param($UserId, $LogonType, $RunLevel); return [pscustomobject]@{ UserId = $UserId; LogonType = $LogonType; RunLevel = $RunLevel } }
        function New-ScheduledTaskSettingsSet {
            param([switch]$StartWhenAvailable, [switch]$AllowStartIfOnBatteries, [switch]$DontStopIfGoingOnBatteries, $MultipleInstances, $ExecutionTimeLimit, $RestartCount, $RestartInterval)
            return [pscustomobject]@{ MultipleInstances = $MultipleInstances; ExecutionTimeLimit = $ExecutionTimeLimit }
        }
        function Register-ScheduledTask {
            param($TaskName, $Action, $Trigger, $Settings, $Principal, $Description, [switch]$Force)
            if ($global:PortalTestFailRegistration) { throw 'Simulated access denied' }
            $global:PortalTestTasks[$TaskName] = [pscustomobject]@{ Actions = @($Action); Settings = $Settings; Principal = $Principal }
        }
        function Stop-ScheduledTask { [CmdletBinding()] param($TaskName) }
        function Start-ScheduledTask { param($TaskName); $global:PortalTestStarted += $TaskName }
        function Unregister-ScheduledTask { [CmdletBinding()] param($TaskName, [switch]$Confirm); $global:PortalTestTasks.Remove($TaskName) }
        function Get-CimInstance { param($ClassName); return @() }

        $installer = Join-Path $testRoot 'scripts\install-portal-windows.ps1'
        $taskInstaller = Join-Path $testRoot 'scripts\install-portal-task.ps1'
        $link = 'https://relay.invalid/test-being/?token=fake-test-token'
        $failed = $false
        try { & $installer -Root $testRoot -ConnectLink 'https://relay.invalid/test-being/' } catch { $failed = $true }
        Assert ($failed -and $global:PortalTestTasks.Count -eq 0) 'invalid link fails before installation'
        & $installer -Root $testRoot -ConnectLink $link -PortalName 'first-name' -TaskName 'CustomPortalTask'
        Assert ((Get-Content (Join-Path $testRoot '.portal-name') -Raw) -eq 'first-name') 'explicit first name'
        Assert (Test-Path -LiteralPath (Join-Path $testRoot 'workspace')) 'first install creates workspace'
        $config = Get-Content -LiteralPath (Join-Path $testRoot 'portal.toml') -Raw
        & $installer -Root $testRoot -ConnectLink ($link.Replace('fake-test-token', 'rotated-test-token'))
        & $taskInstaller -Root $testRoot
        Assert ((Get-Content (Join-Path $testRoot '.portal-name') -Raw) -eq 'first-name') 'reinstall preserves name'
        Assert ($global:PortalTestTasks.Count -eq 1 -and $global:PortalTestTasks.ContainsKey('CustomPortalTask')) 'reinstall preserves custom task'
        Assert ((Get-Content (Join-Path $testRoot 'portal.toml') -Raw) -eq $config) 'reinstall preserves config'
        Assert ($global:PortalTestTasks['CustomPortalTask'].Actions[0].Execute -like '*\wscript.exe') 'task is windowless'
        Assert ($global:PortalTestTasks['CustomPortalTask'].Settings.MultipleInstances -eq 'IgnoreNew') 'scheduler prevents duplicates'
        Assert ($global:PortalTestTasks['CustomPortalTask'].Principal.LogonType -eq 'Interactive') 'task uses installing user'

        $global:PortalTestFailRegistration = $true
        $failed = $false
        try { & $taskInstaller -Root $testRoot -PortalName 'must-not-persist' } catch { $failed = $true }
        Assert $failed 'registration failure reported'
        Assert ((Get-Content (Join-Path $testRoot '.portal-name') -Raw) -eq 'first-name') 'failed install keeps identity'
        $global:PortalTestFailRegistration = $false

        & $taskInstaller -Root $testRoot -TaskName 'RenamedPortalTask'
        Assert ($global:PortalTestTasks.Count -eq 1 -and $global:PortalTestTasks.ContainsKey('RenamedPortalTask')) 'explicit task rename removes old task'
        $global:PortalTestTasks['UnrelatedTask'] = [pscustomobject]@{ Actions = @([pscustomobject]@{ Arguments = 'unrelated.ps1' }) }
        $failed = $false
        try { & $taskInstaller -Root $testRoot -TaskName 'UnrelatedTask' } catch { $failed = $true }
        Assert $failed 'cannot overwrite unrelated task'
        & (Join-Path $testRoot 'scripts\uninstall-portal-task.ps1') -Root $testRoot
        Assert ($global:PortalTestTasks.Count -eq 1 -and $global:PortalTestTasks.ContainsKey('UnrelatedTask')) 'uninstall only removes owned task'
        Write-Output 'PASS: install, reinstall, name/task persistence, hidden action, failed registration, rename, uninstall'
    }

    . (Join-Path $testRoot 'scripts\portal-task-common.ps1')
    $scriptPath = Join-Path $testRoot 'scripts\portal-supervisor.ps1'
    Assert (Test-PortalScriptCommand ('-File "{0}"' -f $scriptPath) $scriptPath) 'quoted exact script matches'
    Assert (-not (Test-PortalScriptCommand ('-File "{0}.backup"' -f $scriptPath) $scriptPath)) 'similar script does not match'
    $supervisorProcess = Launch-Supervisor
    Wait-Until { @(Get-Launches).Count -ge 1 } 'first Portal start'
    $duplicate = Launch-Supervisor
    try {
        Assert ($duplicate.WaitForExit(10000)) 'duplicate supervisor exits'
        Assert ($duplicate.ExitCode -eq 0) 'duplicate supervisor reports success without spawning'
    } finally { $duplicate.Dispose() }
    Assert (@(Get-Launches).Count -eq 1) 'only one Portal created'

    $launches = @(Get-Launches)
    $portalPid = [int]$launches[0].Split('|')[0]
    $portalProcess = Get-Process -Id $portalPid
    try {
        Assert ($portalProcess.Path -eq (Join-Path $testRoot 'target\release\heart-portal.exe')) 'kill target is fixture'
        $portalProcess.Kill()
        Assert ($portalProcess.WaitForExit(5000)) 'fixture exits after kill'
    } finally { $portalProcess.Dispose() }
    Wait-Until { @(Get-Launches).Count -ge 2 } 'Portal restarts after external kill'
    foreach ($launch in @(Get-Launches)) {
        Assert ($launch -like '*|--name|first-name|1') 'restart keeps original name and supervised marker'
    }
    Assert (-not $supervisorProcess.HasExited) 'supervisor survives Portal kill'
    Write-Output 'PASS: duplicate supervisor rejected; crash restart keeps original name'
    Stop-PortalCheckoutProcesses $testRoot
    $supervisorProcess.Dispose()

    New-Item -ItemType File -Path (Join-Path $testRoot 'hold-pipes') | Out-Null
    $countBefore = @(Get-Launches).Count
    $supervisorProcess = Launch-Supervisor
    Wait-Until {
        $log = Join-Path $testRoot 'portal-runtime.log'
        (Test-Path -LiteralPath $log) -and ((Get-Content -LiteralPath $log -Raw) -like '*child holding inherited stdout*')
    } 'fixture child really inherited stdout'
    Wait-Until { @(Get-Launches).Count -ge ($countBefore + 2) } 'restart while kit holds inherited log pipes' 15
    Write-Output 'PASS: inherited stdout/stderr cannot stall supervisor restart'
    Stop-PortalCheckoutProcesses $testRoot
    $supervisorProcess.Dispose()
} finally {
    . (Join-Path $testRoot 'scripts\portal-task-common.ps1')
    Stop-PortalCheckoutProcesses $testRoot
    $resolvedTestRoot = (Resolve-Path -LiteralPath $testRoot).Path
    if (-not $resolvedTestRoot.StartsWith([IO.Path]::GetFullPath($tempBase), [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path $resolvedTestRoot -Leaf) -notlike 'portal Windows test *') { throw 'Unsafe test cleanup path' }
    Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
}
