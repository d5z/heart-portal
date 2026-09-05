param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [string]$PortalName = '',
    [ValidateRange(1, 300)]
    [int]$RestartDelaySeconds = 5
)

$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path -LiteralPath $Root).Path
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')
$exe = Join-Path $Root 'target\release\heart-portal.exe'
$config = Join-Path $Root 'portal.toml'
$linkFile = Join-Path $Root '.portal-connection.url'
$nameFile = Join-Path $Root '.portal-name'
$stdoutLog = Join-Path $Root 'portal-runtime.log'
$stderrLog = Join-Path $Root 'portal-runtime.err.log'

if (-not (Test-Path -LiteralPath $exe)) { throw "Portal binary not found: $exe" }
if (-not (Test-Path -LiteralPath $config)) { throw "Portal config not found: $config" }
if (-not (Test-Path -LiteralPath $linkFile)) { throw "Connection file not found: $linkFile" }

$loomLink = (Get-Content -LiteralPath $linkFile -Raw).Trim()
if ([string]::IsNullOrWhiteSpace($loomLink)) { throw "Connection file is empty: $linkFile" }

if ([string]::IsNullOrWhiteSpace($PortalName) -and (Test-Path -LiteralPath $nameFile)) {
    $PortalName = (Get-Content -LiteralPath $nameFile -Raw).Trim()
}
if ([string]::IsNullOrWhiteSpace($PortalName)) {
    throw "Portal name is not configured. Run install-portal-windows.ps1 or pass -PortalName explicitly."
}

if ($PortalName -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') { throw 'Invalid PortalName.' }

# Prevent a manually launched supervisor and the scheduled task from racing.
# Key by relay host + Being, not by the token: rotating credentials must not
# permit a second supervisor for the same relay identity.
$loomUri = Get-PortalLoomUri $loomLink
$beingId = $loomUri.AbsolutePath.Trim('/').Split('/')[0]
if ([string]::IsNullOrWhiteSpace($beingId)) { throw 'Connection file has no Being ID.' }
$supervisorIdentity = "$($loomUri.Authority.ToLowerInvariant())/$beingId"
$sha256 = [System.Security.Cryptography.SHA256]::Create()
try {
    $identityBytes = [System.Text.Encoding]::UTF8.GetBytes($supervisorIdentity)
    $identityHash = [System.BitConverter]::ToString($sha256.ComputeHash($identityBytes)).Replace('-', '')
} finally {
    $sha256.Dispose()
}
$createdNew = $false
$supervisorMutex = [System.Threading.Mutex]::new($true, "Local\heart-portal-supervisor-$identityHash", [ref]$createdNew)
if (-not $createdNew) {
    $supervisorMutex.Dispose()
    Write-Output 'Another Portal supervisor is already running for this relay/Being; exiting.'
    exit 0
}

try {
    while ($true) {
        $process = $null
        $stdoutStream = $null
        $stderrStream = $null
        try {
            # CreateNoWindow isolates Portal from the supervisor's console.
            # A Ctrl+C/taskkill directed at Portal must never terminate the
            # supervisor that is responsible for bringing it back.
            $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
            $startInfo.FileName = $exe
            $startInfo.Arguments = "--config `"$config`" --name `"$PortalName`""
            $startInfo.WorkingDirectory = $Root
            $startInfo.UseShellExecute = $false
            $startInfo.CreateNoWindow = $true
            $startInfo.RedirectStandardOutput = $true
            $startInfo.RedirectStandardError = $true
            # portal_restart is safe only when an external supervisor is
            # guaranteed to relaunch this process. Keep the credential out of
            # the child command line as well.
            $startInfo.EnvironmentVariables['HEART_PORTAL_SUPERVISED'] = '1'
            $startInfo.EnvironmentVariables['PORTAL_CONNECT_LINK'] = $loomLink

            $stdoutStream = [System.IO.FileStream]::new($stdoutLog, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::ReadWrite)
            $stderrStream = [System.IO.FileStream]::new($stderrLog, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::ReadWrite)
            $process = [System.Diagnostics.Process]::new()
            $process.StartInfo = $startInfo
            if (-not $process.Start()) { throw 'Portal process failed to start.' }
            $stdoutCopy = $process.StandardOutput.BaseStream.CopyToAsync($stdoutStream)
            $stderrCopy = $process.StandardError.BaseStream.CopyToAsync($stderrStream)
            Write-Host "Portal started (PID $($process.Id)); waiting for exit..."
            $process.WaitForExit()
            # A kit can inherit Portal's stdout/stderr and keep the pipe open
            # even after Portal dies. Never let draining logs block recovery.
            if (-not $stdoutCopy.Wait(1000)) { $process.StandardOutput.Close() }
            if (-not $stderrCopy.Wait(1000)) { $process.StandardError.Close() }
            Write-Warning "Portal exited with code $($process.ExitCode); restarting in $RestartDelaySeconds seconds"
        } catch {
            Write-Warning "Portal supervisor error: $($_.Exception.Message); retrying in $RestartDelaySeconds seconds"
        } finally {
            if ($process) {
                # Do not orphan a live child if setup/logging failed after Start.
                try {
                    if (-not $process.HasExited) {
                        $process.Kill()
                        $process.WaitForExit()
                    }
                } catch { Write-Warning "Portal cleanup: $($_.Exception.Message)" }
            }
            if ($stdoutStream) { $stdoutStream.Dispose() }
            if ($stderrStream) { $stderrStream.Dispose() }
            if ($process) { $process.Dispose() }
        }
        Start-Sleep -Seconds $RestartDelaySeconds
    }
} finally {
    $supervisorMutex.ReleaseMutex()
    $supervisorMutex.Dispose()
}
