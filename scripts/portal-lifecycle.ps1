# Windows lifecycle protocol v1. Lock files are never deleted: the open handle,
# not the presence of the file, owns the lock (including across logon sessions).
# Native child processes can inherit PowerShell 7's incompatible module path.
# Resolve utility commands (notably Get-FileHash) from this host's own module.
Import-Module (Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1') -ErrorAction Stop


# Open the object itself and inspect its handle before reading: path metadata
# alone permits a symlink/reparse replacement between the check and the open.
if (-not ('PortalBoundedMetadata' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class PortalBoundedMetadata {
    [StructLayout(LayoutKind.Sequential)]
    struct Info {
        public uint Attributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME Creation, Access, Write;
        public uint Volume, SizeHigh, SizeLow, Links, IndexHigh, IndexLow;
    }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern SafeFileHandle CreateFileW(string name, uint access, uint share,
        IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern bool GetFileInformationByHandle(SafeFileHandle handle, out Info info);
    [DllImport("kernel32.dll", SetLastError=true)]
    static extern uint GetFileType(SafeFileHandle handle);
    public static string Read(string path) {
        const int limit = 1024 * 1024;
        using (var handle = CreateFileW(path, 0x80000000, 7, IntPtr.Zero, 3, 0x00200000, IntPtr.Zero)) {
            if (handle.IsInvalid) {
                int error = Marshal.GetLastWin32Error();
                if (error == 2 || error == 3) throw new FileNotFoundException("Portal metadata is absent.");
                throw new IOException("Cannot open Portal metadata.", unchecked((int)(0x80070000u | (uint)error)));
            }
            Info info;
            if (GetFileType(handle) != 1 || !GetFileInformationByHandle(handle, out info) ||
                (info.Attributes & (0x400 | 0x10)) != 0 || info.SizeHigh != 0 || info.SizeLow > limit)
                throw new IOException("Portal metadata must be a regular non-link file no larger than 1 MiB.");
            using (var stream = new FileStream(handle, FileAccess.Read))
            using (var bytes = new MemoryStream()) {
                var buffer = new byte[4096];
                int count;
                while ((count = stream.Read(buffer, 0, Math.Min(buffer.Length, limit + 1 - (int)bytes.Length))) != 0) {
                    bytes.Write(buffer, 0, count);
                    if (bytes.Length > limit) throw new IOException("Portal metadata exceeds 1 MiB.");
                }
                return new UTF8Encoding(false, true).GetString(bytes.ToArray()).TrimStart('\uFEFF');
            }
        }
    }
}
'@
}

function Read-PortalText([string]$Path) {
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        try { return [PortalBoundedMetadata]::Read($Path) }
        catch [IO.FileNotFoundException] { return $null }
        catch [IO.IOException] {
            if (($_.Exception.HResult -band 0xffff) -notin @(32, 33) -or $attempt -eq 19) { throw }
            Start-Sleep -Milliseconds 50
        }
    }
}

function Get-PortalExecutable([string]$Root) {
    $saved = Join-Path $Root '.portal-executable'
    if (Test-Path -LiteralPath $saved) {
        $relative = (Read-PortalText $saved).Trim()
        if ($relative -notin @('heart-portal.exe', 'heart-portal-windows-x86_64.exe', 'heart-portal-windows-aarch64.exe', 'target\release\heart-portal.exe')) { throw 'Invalid saved Portal executable path.' }
        return (Join-Path $Root $relative)
    }
    $flat = Join-Path $Root 'heart-portal.exe'
    if (Test-Path -LiteralPath $flat) { return $flat }
    return (Join-Path $Root 'target\release\heart-portal.exe')
}

function Enter-PortalMaintenance([string]$Root) {
    $owner = Open-PortalLock $Root '.portal-upgrade.lock'
    if (-not $owner) { throw 'Portal upgrade is in progress; retry after it completes.' }
    try {
        $gate = Open-PortalLock $Root '.portal-lifecycle.lock' 15
        if (-not $gate) { throw 'Portal lifecycle is busy; retry later.' }
        return @($owner, $gate)
    } catch { $owner.Dispose(); throw }
}

function Open-PortalLock([string]$Root, [string]$Name, [int]$TimeoutSeconds = 0) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        try {
            return [IO.File]::Open((Join-Path $Root $Name), [IO.FileMode]::OpenOrCreate,
                [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        } catch [IO.IOException] {
            # Only sharing/lock violations mean another lifecycle operation owns it.
            if (($_.Exception.HResult -band 0xffff) -notin @(32, 33)) { throw }
            if ($timer.Elapsed.TotalSeconds -ge $TimeoutSeconds) { return $null }
            Start-Sleep -Milliseconds 100
        }
    } while ($true)
}

function New-PortalFileSecurity {
    $security = [Security.AccessControl.FileSecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    foreach ($sid in @([Security.Principal.WindowsIdentity]::GetCurrent().User,
                       [Security.Principal.SecurityIdentifier]::new('S-1-5-18'))) {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $sid, [Security.AccessControl.FileSystemRights]::FullControl,
            [Security.AccessControl.AccessControlType]::Allow))
    }
    return $security
}

function Protect-PortalFile([string]$Path) {
    if (Test-Path -LiteralPath $Path) {
        $item = Get-Item -LiteralPath $Path -Force
        if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Managed Portal credentials must be regular files.'
        }
        [IO.File]::SetAccessControl($Path, (New-PortalFileSecurity))
    }
}

function Write-PortalPrivateText([string]$Path, [string]$Value) {
    $temp = "$Path.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($Value)
        $security = New-PortalFileSecurity
        # Apply a protected DACL at creation, before any secret reaches disk.
        $stream = [IO.FileStream]::new($temp, [IO.FileMode]::CreateNew,
            [Security.AccessControl.FileSystemRights]::FullControl, [IO.FileShare]::None,
            4096, [IO.FileOptions]::None, $security)
        try {
            $stream.SetAccessControl($security) # Fail closed if the volume cannot enforce ACLs.
            $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true)
        } finally { $stream.Dispose() }
        if (Test-Path -LiteralPath $Path) {
            # File.Replace retains the destination DACL; repair legacy broad
            # permissions first, otherwise a private temp file is insufficient.
            Protect-PortalFile $Path
            [IO.File]::Replace($temp, $Path, [NullString]::Value)
        }
        else { [IO.File]::Move($temp, $Path) }
    } finally {
        if (Test-Path -LiteralPath $temp) { [IO.File]::Delete($temp) }
    }
}

function Write-PortalJson([string]$Path, $Value) {
    Write-PortalPrivateText $Path ($Value | ConvertTo-Json -Depth 8 -Compress)
}

function Read-PortalJson([string]$Path) {
    if ([IO.Path]::GetFileName($Path) -in @('.portal-launch.json', '.portal-direct.json', '.portal-upgrade.json', 'request.json')) {
        Protect-PortalFile $Path
    }
    $text = Read-PortalText $Path
    if ($null -eq $text) { return $null }
    return ($text | ConvertFrom-Json)
}

function Set-PortalUpgradeStatus([string]$Root, [string]$State, [string]$Message, [string]$Version = '') {
    Write-PortalJson (Join-Path $Root '.portal-upgrade-status.json') @{
        state = $State; message = $Message; version = $Version; time = [DateTime]::UtcNow.ToString('o')
    }
}

# Callers hold the lifecycle gate when using this handle to take ownership.
# PID alone is insufficient: Windows reuses it, and start/upgrade CLIs use the
# same executable. Only a published runtime with the same creation time counts.
function Get-PortalRecordedProcess([string]$Root, $Runtime) {
    if (-not $Runtime -or $Runtime.protocol -ne 1 -or -not $Runtime.pid -or
        -not $Runtime.started -or -not $Runtime.nonce) { return $null }
    $process = Get-Process -Id $Runtime.pid -ErrorAction SilentlyContinue
    if (-not $process) { return $null }
    try {
        if (-not $process.HasExited -and $process.StartTime.ToUniversalTime().Ticks -eq $Runtime.started -and
            $process.Path.Equals((Get-PortalExecutable $Root), [StringComparison]::OrdinalIgnoreCase)) {
            # Open the process handle before returning so waits still work if
            # this non-child exits before the new supervisor starts waiting.
            [void]$process.Handle
            return $process
        }
    } catch { $process.Dispose(); throw }
    $process.Dispose()
    return $null
}

function Test-SavedSupervisor($Runtime) {
    if (-not $Runtime.supervisor_pid) { return $false }
    $process = Get-Process -Id $Runtime.supervisor_pid -ErrorAction SilentlyContinue
    if (-not $process) { return $false }
    try { return -not $process.HasExited -and $process.StartTime.ToUniversalTime().Ticks -eq $Runtime.supervisor_started }
    finally { $process.Dispose() }
}

function Stop-PortalRuntime([string]$Root, [string]$Exe = (Get-PortalExecutable $Root)) {
    # Never use taskkill /IM: other installations and concurrent upgrade CLIs
    # must survive. Recheck the actual process path after enumerating PIDs.
    $items = @(Get-CimInstance Win32_Process | Where-Object {
        $_.ExecutablePath -and $_.ExecutablePath.Equals($Exe, [StringComparison]::OrdinalIgnoreCase) -and
        $_.ProcessId -ne $PID -and $_.CommandLine -notmatch '(?i)(?:^|\s)"?(?:upgrade|--upgrade|--version|stop|status)"?(?:\s|$)'
    })
    foreach ($item in $items) {
        $process = Get-Process -Id $item.ProcessId -ErrorAction SilentlyContinue
        if (-not $process) { continue }
        try {
            if (-not $process.HasExited -and $process.Path.Equals($Exe, [StringComparison]::OrdinalIgnoreCase)) {
                $process.Kill()
                if (-not $process.WaitForExit(10000)) { throw "Portal PID $($item.ProcessId) did not exit." }
            }
        } catch { if (-not $process.HasExited) { throw } }
        finally { $process.Dispose() }
    }
}

function Stop-PortalSupervisorCore([string]$Root) {
    $core = Join-Path $Root 'scripts\portal-supervisor.ps1'
    $pattern = '(?i)(?:^|[\s"''])' + [regex]::Escape($core) + '(?:$|[\s"''])'
    $items = @(Get-CimInstance Win32_Process | Where-Object {
        $_.ProcessId -ne $PID -and $_.Name -in @('powershell.exe', 'pwsh.exe') -and $_.CommandLine -match $pattern
    })
    foreach ($item in $items) {
        $process = Get-Process -Id $item.ProcessId -ErrorAction SilentlyContinue
        if ($process) {
            try { $process.Kill(); if (-not $process.WaitForExit(10000)) { throw 'Supervisor did not stop.' } }
            catch { if (-not $process.HasExited) { throw } }
            finally { $process.Dispose() }
        }
    }
}

function Ensure-PortalBootstrap([string]$Root, $Runtime, [string]$Payload) {
    if ($Runtime.bootstrap_pid) {
        $process = Get-Process -Id $Runtime.bootstrap_pid -ErrorAction SilentlyContinue
        if ($process) {
            try {
                $item = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.Id)"
                $expected = [regex]::Escape((Join-Path $Root 'scripts\portal-supervisor-bootstrap.ps1'))
                if ($item.CommandLine -match ('(?i)(?:^|[\s"''])' + $expected + '(?:$|[\s"''])')) { return }
            } finally { $process.Dispose() }
        }
    }
    # Bootstrap v1 is deliberately outside the mutable supervisor payload.
    # It only holds the launch gate and restarts/recoveries the versioned core.
    $bootstrap = Join-Path $Root 'scripts\portal-supervisor-bootstrap.ps1'
    if (-not (Test-Path -LiteralPath $bootstrap)) {
        [IO.File]::Copy((Join-Path $Payload 'portal-supervisor-bootstrap.ps1'), $bootstrap)
    }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $launcher = Join-Path $Root 'scripts\portal-background-v1.exe'
    $sourceLauncher = Join-Path $Payload 'portal-background-v1.exe'
    if (-not (Test-Path -LiteralPath $launcher) -and (Test-Path -LiteralPath $sourceLauncher)) {
        [IO.File]::Copy($sourceLauncher, $launcher)
    }
    if (Test-Path -LiteralPath $launcher) {
        $info.FileName = $launcher
        $info.Arguments = '-File ' + (ConvertTo-PortalArgument $bootstrap) + ' -Root ' + (ConvertTo-PortalArgument $Root)
    } else {
        # Recovery of older payloads must remain possible before a new start
        # has exported the native helper into this installation.
        $info.FileName = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $info.Arguments = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' + (ConvertTo-PortalArgument $bootstrap) + ' -Root ' + (ConvertTo-PortalArgument $Root)
    }
    # This fallback may be launched by the short-lived start helper with piped
    # stdout. ShellExecute detaches those handles so that helper can finish.
    $info.UseShellExecute = $true
    $info.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $child = [Diagnostics.Process]::Start($info)
    if (-not $child) { throw 'Could not start the stable supervisor bootstrap.' }
    $child.Dispose()
}

function Restore-PortalUpgrade([string]$Root, $Journal) {
    $target = [IO.Path]::GetFullPath($Journal.target)
    $expected = [IO.Path]::GetFullPath((Join-Path $Root $Journal.relative_target))
    if ($Journal.relative_target -notin @('heart-portal.exe', 'heart-portal-windows-x86_64.exe', 'heart-portal-windows-aarch64.exe', 'target\release\heart-portal.exe') -or $target -ne $expected) {
        throw 'Invalid upgrade journal target.'
    }
    $backup = [IO.Path]::GetFullPath($Journal.backup)
    if ([IO.Path]::GetDirectoryName($backup) -ne [IO.Path]::GetDirectoryName($target) -or
        [IO.Path]::GetFileName($backup) -notmatch '^heart-portal\.bak\.[a-f0-9]{32}\.exe$') {
        throw 'Invalid upgrade journal backup.'
    }
    if ($Journal.support) { Stop-PortalSupervisorCore $Root }
    Stop-PortalRuntime $Root $target
    if (Test-Path -LiteralPath $backup) {
        if (Test-Path -LiteralPath $target) {
            [IO.File]::Replace($backup, $target, "$target.failed.$([guid]::NewGuid().ToString('N'))")
        } else { [IO.File]::Move($backup, $target) }
    }
    if (-not (Test-Path -LiteralPath $target)) { throw 'Neither the previous binary nor its backup is available.' }
    foreach ($entry in $Journal.support) {
        if ($entry.name -notin @('portal-lifecycle.ps1', 'portal-supervisor.ps1', 'portal-supervisor-hidden.vbs')) { throw 'Invalid supervisor backup entry.' }
        $destination = Join-Path $Root "scripts\$($entry.name)"
        $source = [IO.Path]::GetFullPath($entry.backup)
        $stageRoot = [IO.Path]::GetFullPath((Join-Path $Root '.portal-upgrades')) + '\'
        if (-not $source.StartsWith($stageRoot, [StringComparison]::OrdinalIgnoreCase)) { throw 'Invalid supervisor backup path.' }
        if ($entry.existed) {
            $temp = "$destination.restore.$([guid]::NewGuid().ToString('N'))"
            [IO.File]::Copy($source, $temp)
            if (Test-Path -LiteralPath $destination) { [IO.File]::Replace($temp, $destination, [NullString]::Value) }
            else { [IO.File]::Move($temp, $destination) }
        } else { [IO.File]::Delete($destination) }
    }
    Set-PortalUpgradeStatus $Root 'rolled_back' 'Previous binary restored; supervisor will restart it.' $Journal.version
    [IO.File]::Delete((Join-Path $Root '.portal-upgrade.json'))
}

# Caller holds the lifecycle lock. A crashed/killed updater releases its OS lock;
# a supervisor then repairs the journal before launching any binary.
function Repair-PortalInterruptedUpgrade([string]$Root) {
    $journalPath = Join-Path $Root '.portal-upgrade.json'
    if (-not (Test-Path -LiteralPath $journalPath)) { return }
    $owner = Open-PortalLock $Root '.portal-upgrade.lock'
    if (-not $owner) { return }
    try {
        $journal = Read-PortalJson $journalPath
        Restore-PortalUpgrade $Root $journal
        return [bool]$journal.support
    }
    finally { $owner.Dispose() }
}

function Test-PortalReady([string]$Root, [string]$Version = '') {
    $ready = Read-PortalJson (Join-Path $Root '.portal-ready.json')
    $runtime = Read-PortalJson (Join-Path $Root '.portal-runtime.json')
    if (-not $ready -or -not $runtime -or $ready.pid -ne $runtime.pid -or
        $ready.nonce -ne $runtime.nonce -or ($Version -and $ready.version -ne $Version)) { return $false }
    $process = Get-Process -Id $runtime.pid -ErrorAction SilentlyContinue
    if (-not $process) { return $false }
    try {
        return -not $process.HasExited -and
            $process.Path.Equals((Get-PortalExecutable $Root), [StringComparison]::OrdinalIgnoreCase) -and
            ((-not $runtime.started) -or $process.StartTime.ToUniversalTime().Ticks -eq $runtime.started)
    } finally { $process.Dispose() }
}

function ConvertTo-PortalArgument([string]$Value) {
    # Standard Windows argv quoting, used directly by CreateProcess (no shell).
    return '"' + [regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
}

function Start-PortalDirect([string]$Root, $Launch) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = Get-PortalExecutable $Root
    $info.Arguments = (@($Launch.arguments | ForEach-Object { ConvertTo-PortalArgument $_ }) -join ' ')
    $info.WorkingDirectory = $Launch.working_directory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    foreach ($entry in $Launch.environment.PSObject.Properties) { $info.EnvironmentVariables[$entry.Name] = [string]$entry.Value }
    $info.EnvironmentVariables.Remove('HEART_PORTAL_SUPERVISED')
    $info.EnvironmentVariables['HEART_PORTAL_READY_FILE'] = Join-Path $Root '.portal-ready.json'
    $info.EnvironmentVariables['HEART_PORTAL_READY_NONCE'] = [guid]::NewGuid().ToString('N')
    [IO.File]::Delete((Join-Path $Root '.portal-ready.json'))
    $process = [Diagnostics.Process]::Start($info)
    if (-not $process) { throw 'Failed to restart the direct Portal process.' }
    $process.Dispose()
}

function Wait-PortalReady([string]$Root, [string]$Version, [int]$TimeoutSeconds = 60) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $stable = $null
    $readyPid = 0
    do {
        if (Test-PortalReady $Root $Version) {
            $currentPid = (Read-PortalJson (Join-Path $Root '.portal-runtime.json')).pid
            if (-not $stable -or $currentPid -ne $readyPid) { $stable = [Diagnostics.Stopwatch]::StartNew(); $readyPid = $currentPid }
            if ($stable.Elapsed.TotalSeconds -ge 5) { return }
        } else { $stable = $null }
        Start-Sleep -Milliseconds 200
    } while ($timer.Elapsed.TotalSeconds -lt $TimeoutSeconds)
    throw "Portal did not become locally ready and remain running within $TimeoutSeconds seconds."
}

function Assert-PortalCandidate([string]$Path, [string]$Version) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Path
    $info.Arguments = '--version'
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    try {
        if (-not $process.Start()) { throw 'Cannot execute downloaded binary.' }
        $output = $process.StandardOutput.ReadToEndAsync()
        $errors = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(15000)) { $process.Kill(); throw 'Downloaded binary version check timed out.' }
        if (-not $output.Wait(1000) -or -not $errors.Wait(1000) -or $process.ExitCode -ne 0 -or
            $output.Result.Trim() -ne "heart-portal $Version") { throw 'Downloaded binary version check failed.' }
    } finally {
        try { if (-not $process.HasExited) { $process.Kill() } } catch {}
        $process.Dispose()
    }
}

function Export-PortalCandidateSupport([string]$Candidate, [string]$Payload) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Candidate
    $info.Arguments = '--export-windows-runtime ' + (ConvertTo-PortalArgument $Payload)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $process = [Diagnostics.Process]::Start($info)
    try {
        if (-not $process.WaitForExit(15000)) { $process.Kill(); throw 'Supervisor payload export timed out.' }
        if ($process.ExitCode -ne 0) { throw 'New binary does not support Windows lifecycle payload export.' }
    } finally { $process.Dispose() }
    foreach ($name in @('portal-lifecycle.ps1', 'portal-supervisor.ps1', 'portal-supervisor-hidden.vbs', 'portal-supervisor-bootstrap.ps1')) {
        if (-not (Test-Path -LiteralPath (Join-Path $Payload $name))) { throw "Incomplete supervisor payload: $name" }
    }
    # Parse before any process is stopped. Syntax errors must leave the current
    # Portal and supervisor completely untouched.
    foreach ($name in @('portal-lifecycle.ps1', 'portal-supervisor.ps1', 'portal-supervisor-bootstrap.ps1')) {
        $tokens = $null; $errors = $null
        [void][Management.Automation.Language.Parser]::ParseFile((Join-Path $Payload $name), [ref]$tokens, [ref]$errors)
        if ($errors.Count) { throw "Invalid PowerShell in candidate payload: $name" }
    }
}

function Invoke-PortalUpgrade($Request) {
    $root = (Resolve-Path -LiteralPath $Request.root).Path
    $owner = Open-PortalLock $root '.portal-upgrade.lock'
    if (-not $owner) { throw 'Another Portal upgrade is in progress.' }
    $gate = $null
    $journal = $null
    $direct = $null
    $payload = $null
    try {
        $target = Get-PortalExecutable $root
        if ($target -ne $Request.target) { throw 'Upgrade target does not match this installation.' }
        if ((Get-FileHash -LiteralPath $Request.candidate -Algorithm SHA256).Hash -ne $Request.sha256) {
            throw 'Downloaded binary checksum changed before installation.'
        }
        Assert-PortalCandidate $Request.candidate $Request.version
        $gate = Open-PortalLock $root '.portal-lifecycle.lock' 15
        if (-not $gate) { throw 'Another install/uninstall operation is in progress.' }
        if (Test-Path -LiteralPath (Join-Path $root '.portal-upgrade.json')) {
            throw 'An interrupted upgrade needs supervisor recovery before retrying.'
        }
        # The supervisor publishes this only after entering the v1 launch gate.
        $runtime = Read-PortalJson (Join-Path $root '.portal-runtime.json')
        if (-not $runtime -or $runtime.protocol -ne 1) {
            # Upgrade an installation using the original supervisor too. The
            # worker puts the stable bootstrap in place before stopping it.
            $core = Join-Path $root 'scripts\portal-supervisor.ps1'
            $pattern = '(?i)(?:^|[\s"''])' + [regex]::Escape($core) + '(?:$|[\s"''])'
            $legacy = @(Get-CimInstance Win32_Process | Where-Object {
                $_.Name -in @('powershell.exe', 'pwsh.exe') -and $_.CommandLine -match $pattern
            })
            if ($legacy.Count -eq 1) {
                $process = Get-Process -Id $legacy[0].ProcessId -ErrorAction Stop
                try { $runtime = [pscustomobject]@{ protocol = 1; supervisor_pid = $process.Id; supervisor_started = $process.StartTime.ToUniversalTime().Ticks } }
                finally { $process.Dispose() }
            }
        }
        if (-not $runtime -or $runtime.protocol -ne 1) { throw 'Run this Portal version once before upgrading, so its launch settings can be saved.' }
        if ($runtime.supervisor_pid) {
            $supervisorProcess = Get-Process -Id $runtime.supervisor_pid -ErrorAction SilentlyContinue
            if (-not $supervisorProcess) { throw 'Portal supervisor is not running.' }
            try {
                if ($supervisorProcess.StartTime.ToUniversalTime().Ticks -ne $runtime.supervisor_started) { throw 'Portal supervisor identity changed.' }
            } finally { $supervisorProcess.Dispose() }
            $payload = Join-Path ([IO.Path]::GetDirectoryName($Request.candidate)) 'support'
            Export-PortalCandidateSupport $Request.candidate $payload
        } else {
            $direct = Read-PortalJson (Join-Path $root '.portal-direct.json')
            if (-not $direct -or $direct.nonce -ne $runtime.nonce) { throw 'Direct Portal launch settings are missing or stale.' }
        }
        # Acknowledge ONLY after validation and exclusive handoff. The CLI exits
        # to release its own executable, while this independent process owns work.
        Set-PortalUpgradeStatus $root 'waiting_for_exit' 'Updater accepted; waiting for upgrade CLI to exit.' $Request.version
        Write-PortalJson $Request.ack @{ accepted = $true }
        $parent = Get-Process -Id $Request.parent_pid -ErrorAction SilentlyContinue
        if ($parent) {
            try {
                if (-not $parent.WaitForExit(30000)) { throw 'Upgrade CLI has not exited; existing Portal left running.' }
            } finally { $parent.Dispose() }
        }
        $backup = Join-Path ([IO.Path]::GetDirectoryName($target)) ("heart-portal.bak.$([guid]::NewGuid().ToString('N')).exe")
        $relative = $target.Substring($root.TrimEnd('\').Length + 1)
        $journal = @{
            target = $target; relative_target = $relative; backup = $backup; version = $Request.version
            recovery_script = (Join-Path ([IO.Path]::GetDirectoryName($Request.candidate)) 'portal-upgrade-worker.ps1')
            direct = $direct
        }
        if ($payload) {
            $supportBackup = Join-Path ([IO.Path]::GetDirectoryName($Request.candidate)) 'support-backup'
            [IO.Directory]::CreateDirectory($supportBackup) | Out-Null
            $journal.support = @()
            foreach ($name in @('portal-lifecycle.ps1', 'portal-supervisor.ps1', 'portal-supervisor-hidden.vbs')) {
                $old = Join-Path $root "scripts\$name"
                $saved = Join-Path $supportBackup $name
                $existed = Test-Path -LiteralPath $old
                if ($existed) { [IO.File]::Copy($old, $saved) }
                $journal.support += @{ name = $name; backup = $saved; existed = $existed }
            }
        }
        Write-PortalJson (Join-Path $root '.portal-upgrade.json') $journal
        Set-PortalUpgradeStatus $root 'replacing' 'Stopping this installation and replacing its executable.' $Request.version
        if ($payload) {
            # The bootstrap stays alive, including if this worker is killed.
            Ensure-PortalBootstrap $root $runtime $payload
            Stop-PortalSupervisorCore $root
        }
        Stop-PortalRuntime $root $target
        # One filesystem operation replaces the target and retains its backup:
        # there is no intentional interval with a missing heart-portal.exe.
        [IO.File]::Replace($Request.candidate, $target, $backup)
        foreach ($entry in $journal.support) {
            $destination = Join-Path $root "scripts\$($entry.name)"
            $source = Join-Path $payload $entry.name
            if (Test-Path -LiteralPath $destination) { [IO.File]::Replace($source, $destination, [NullString]::Value) }
            else { [IO.File]::Move($source, $destination) }
        }
        [IO.File]::Delete((Join-Path $root '.portal-ready.json'))
        Set-PortalUpgradeStatus $root 'verifying' 'Waiting for the supervisor to start the new version.' $Request.version
        $gate.Dispose(); $gate = $null
        if ($direct) { Start-PortalDirect $root $direct }
        Wait-PortalReady $root $Request.version
        if ($payload -and (Read-PortalJson (Join-Path $root '.portal-runtime.json')).supervisor_hash -ne
            (Get-FileHash -LiteralPath (Join-Path $root 'scripts\portal-supervisor.ps1')).Hash) {
            throw 'The running supervisor does not match the new payload.'
        }
        $gate = Open-PortalLock $root '.portal-lifecycle.lock' 15
        if (-not $gate) { throw 'Cannot acquire lifecycle lock to commit upgrade.' }
        # Removing the journal commits the transaction; the previous exe remains
        # as a backup. Recovery is conservative until this point.
        [IO.File]::Delete((Join-Path $root '.portal-upgrade.json'))
        $journal = $null
        Set-PortalUpgradeStatus $root 'succeeded' 'New version is locally ready and running under supervision.' $Request.version
    } catch {
        $failure = $_.Exception.Message
        if ($journal) {
            if (-not $gate) { $gate = Open-PortalLock $root '.portal-lifecycle.lock' 15 }
            if ($gate) {
                try {
                    Restore-PortalUpgrade $root $journal
                    $gate.Dispose(); $gate = $null
                    if ($direct) { Start-PortalDirect $root $direct }
                    Wait-PortalReady $root ''
                    Set-PortalUpgradeStatus $root 'rolled_back' "Upgrade failed: $failure Previous version is running." $Request.version
                } catch {
                    Set-PortalUpgradeStatus $root 'recovery_required' "Upgrade failed: $failure Recovery: $($_.Exception.Message)" $Request.version
                }
            } else {
                Set-PortalUpgradeStatus $root 'recovery_required' "Upgrade failed: $failure Supervisor will recover the journal when the lock is available." $Request.version
            }
        } else { Set-PortalUpgradeStatus $root 'failed' $failure $Request.version }
        throw $failure
    } finally {
        if ($gate) { $gate.Dispose() }
        $owner.Dispose()
    }
}
