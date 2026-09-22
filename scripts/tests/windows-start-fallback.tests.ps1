# Exercise scheduler denial without changing any real task or Windows policy.
param([string]$Binary = (Join-Path $PSScriptRoot '..\..\target\release\heart-portal.exe'))
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
. (Join-Path $repo 'scripts\portal-task-common.ps1')
$tempBase = [IO.Path]::GetTempPath()
$testRoot = Join-Path $tempBase ('portal start fallback-' + [guid]::NewGuid().ToString('N'))
$stage = Join-Path $testRoot '.portal-start\fixture'
$support = Join-Path $stage 'support'
[IO.Directory]::CreateDirectory($support) | Out-Null
$helper = $null
try {
    $exe = Join-Path $testRoot 'heart-portal.exe'
    Add-Type -TypeDefinition ([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'fake-portal.cs'))) -OutputAssembly $exe -OutputType ConsoleApplication
    foreach ($name in @('portal-lifecycle.ps1','portal-task-common.ps1','portal-start-worker.ps1')) {
        Copy-Item -LiteralPath (Join-Path $repo "scripts\$name") -Destination $stage
    }
    foreach ($name in @('portal-lifecycle.ps1','portal-supervisor.ps1','portal-supervisor-bootstrap.ps1','portal-supervisor-hidden.vbs')) {
        Copy-Item -LiteralPath (Join-Path $repo "scripts\$name") -Destination $support
    }
    & $Binary --export-windows-launcher (Join-Path $support 'portal-background-v1.exe') | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Cannot export the native background launcher fixture.' }
    # Imported after the shared definitions, these replace only this helper's
    # scheduler calls. No machine policy or scheduled task is modified.
    Add-Content -LiteralPath (Join-Path $stage 'portal-task-common.ps1') -Value @'
function Get-ScheduledTask { [CmdletBinding()] param($TaskName); return $null }
function New-ScheduledTaskAction { param($Execute,$Argument,$WorkingDirectory); throw 'Fixture: scheduler access denied' }
'@
    $launch = @{ protocol=1; identity=('standalone/fallback-' + [guid]::NewGuid()); name='fallback'; arguments=@('--config',(Join-Path $testRoot 'portal.toml'),'--name','fallback'); working_directory=$testRoot; environment=@{} }
    [IO.File]::WriteAllText([string]$launch.arguments[1], '# fixture')
    Write-PortalJson (Join-Path $stage 'request.json') @{ action='start'; root=$testRoot; target=$exe; launch=$launch; initialize_config=$false; explicit=$false; parent_pid=$PID; version='0.8.0' }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' + (ConvertTo-PortalArgument (Join-Path $stage 'portal-start-worker.ps1'))
    $info.UseShellExecute=$false; $info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
    $helper = [Diagnostics.Process]::Start($info)
    $output = $helper.StandardOutput.ReadToEndAsync()
    $errors = $helper.StandardError.ReadToEndAsync()
    if (-not $helper.WaitForExit(30000)) { throw 'Start fallback helper timed out.' }
    if (-not $output.Wait(2000) -or -not $errors.Wait(2000)) { throw 'Fallback guardian inherited CLI output pipes.' }
    if ($helper.ExitCode -ne 0) { throw "Fallback failed: $($errors.Result)" }
    $status = Read-PortalJson (Join-Path $testRoot '.portal-start-status.json')
    if ($status.autostart -or $status.warning -notlike '*scheduler access denied*' -or -not (Test-PortalReady $testRoot '0.8.0')) { throw 'Fallback status is incorrect.' }
    $runtime = Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')
    Stop-Process -Id $runtime.pid
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        if ((Test-PortalReady $testRoot '0.8.0') -and (Read-PortalJson (Join-Path $testRoot '.portal-runtime.json')).pid -ne $runtime.pid) { break }
        if ($timer.Elapsed.TotalSeconds -gt 20) { throw 'Fallback guardian did not recover the Portal.' }
        Start-Sleep -Milliseconds 200
    } while ($true)
    Write-Output 'PASS: denied scheduler still starts a detached guardian, reports missing logon recovery, and recovers crashes'
} finally {
    if ($helper) { if (-not $helper.HasExited) { $helper.Kill(); [void]$helper.WaitForExit(5000) }; $helper.Dispose() }
    Stop-PortalCheckoutProcesses $testRoot
    $resolved = [IO.Path]::GetFullPath($testRoot)
    if (-not $resolved.StartsWith([IO.Path]::GetFullPath($tempBase), [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolved -Leaf) -notlike 'portal start fallback-*') { throw 'Unsafe fallback cleanup path.' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
