# Passed as a Unicode -Command string by the EXE; shares its exported lifecycle reader.
# This process only reads files. It must never own the EXE or lifecycle locks.
param([string]$Root = $env:HEART_PORTAL_CONSOLE_ROOT)
$ErrorActionPreference = 'Stop'
. (Join-Path $Root 'scripts\portal-lifecycle.ps1')
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

function Read-ConsoleJson([string]$Name) {
    try { return ((Read-PortalText (Join-Path $Root $Name)) | ConvertFrom-Json) }
    catch { return $null }
}

function Quote-ConsoleArgument([string]$Value) { return "'" + $Value.Replace("'", "''") + "'" }

function Show-ConnectionStatus([string]$State, $Launch) {
    $label = switch ($State) {
        'local' { '未配置 Being（仅本地服务）' }
        'connecting' { '正在连接 Being' }
        'connected' { '已连接 Being' }
        'retrying' { 'Being 未连接，等待自动重试' }
        'handshake_rejected' { 'Relay 拒绝了 Portal 握手，等待自动重试' }
        'invalid' { 'Being 链接无效，请检查完整链接和 token' }
        'stopped' { 'Portal 未运行' }
        default { '等待当前进程的连接状态' }
    }
    $color = if ($State -eq 'connected') { 'Green' } elseif ($State -in @('local','retrying','handshake_rejected','invalid')) { 'Yellow' } else { 'Cyan' }
    if (-not [Console]::IsOutputRedirected) {
        try { [Console]::Title = "Heart Portal | $label" } catch { }
    }
    Write-Host "`n========== Being 连接状态：$label ==========" -ForegroundColor $color
    if ($State -eq 'local' -and $Launch) {
        $relative = (Read-PortalText (Join-Path $Root '.portal-executable')).Trim()
        $exeCommand = '& ' + (Quote-ConsoleArgument (Join-Path $Root $relative))
        Write-Output '请另开 PowerShell 窗口，依次执行下面三行（此日志窗口不接收命令）：'
        Write-Output '$beingLink = Read-Host ''请粘贴从 Beings 复制的完整连接链接（包含 token=）'''
        Write-Output "$exeCommand stop"
        Write-Output ($exeCommand + ' --config ' + (Quote-ConsoleArgument $Launch.arguments[1]) + ' --name ' + (Quote-ConsoleArgument $Launch.name) + ' --connect $beingLink')
        Write-Output '--connect 接收完整链接，不能只填写 token；以上命令保留当前配置和 Portal 名称。'
    } elseif ($State -eq 'connected') {
        Write-Output '连接握手成功，Being 现在可以通过此 Portal 调用本机工具。'
    } elseif ($State -in @('retrying', 'handshake_rejected')) {
        Write-Output '连接失败或已断开，Portal 会自动重试；具体原因见上方运行日志。'
    }
    Write-Host '============================================================' -ForegroundColor $color
}

$streams = @{}
foreach ($name in @('portal-runtime.log', 'portal-runtime.err.log')) {
    $streams[$name] = @{ offset=0L; pending=''; decoder=[Text.Encoding]::UTF8.GetDecoder(); initial=$true }
}
$generation = ''
$lastState = ''
$connection = 'unknown'
$shownConnection = ''
$launch = $null
$secrets = @()
try {
    while ($true) {
        $runtime = Read-ConsoleJson '.portal-runtime.json'
        $nonce = [string]$runtime.nonce
        if ($nonce -and $nonce -ne $generation) {
            $generation = $nonce
            $connection = 'unknown'; $shownConnection = ''
            foreach ($stream in $streams.Values) {
                $stream.offset = 0L; $stream.pending = ''; $stream.decoder.Reset(); $stream.initial = $true
            }
            $launch = Read-ConsoleJson '.portal-launch.json'
            $secrets = @()
            $link = [string]$launch.environment.PORTAL_CONNECT_LINK
            if ($link) {
                $secrets += $link
                try {
                    foreach ($pair in ([Uri]$link).Query.TrimStart('?').Split('&')) {
                        if ($pair.StartsWith('token=')) {
                            $token = $pair.Substring(6)
                            if ($token) { $secrets += $token; $secrets += [Uri]::UnescapeDataString($token) }
                        }
                    }
                } catch { }
            }
            if ($launch.environment.PORTAL_MCP_TOKEN) { $secrets += [string]$launch.environment.PORTAL_MCP_TOKEN }
            Write-Output "[运行] Portal 进程 $($runtime.pid)；正在读取运行日志。"
        }
        $process = if ($runtime.pid) { Get-Process -Id $runtime.pid -ErrorAction SilentlyContinue } else { $null }
        $alive = $false
        if ($process) {
            try { $alive = -not $process.HasExited -and $process.StartTime.ToUniversalTime().Ticks -eq $runtime.started }
            finally { $process.Dispose() }
        }
        $state = if ($alive) { 'running' } else { 'waiting' }
        if ($state -ne $lastState) {
            if (-not $alive) { Write-Output '[运行] Portal 当前未运行，正在等待守护恢复或后续启动。可关闭此日志窗口。' }
            $lastState = $state
        }
        foreach ($name in $streams.Keys) {
            $stream = $streams[$name]
            $file = $null
            try {
                $file = [IO.File]::Open((Join-Path $Root $name), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
                if ($file.Length -lt $stream.offset) {
                    $stream.offset = 0L; $stream.pending = ''; $stream.decoder.Reset(); $stream.initial = $true
                }
                $skipPartial = $false
                if ($stream.initial) {
                    # Show recent history without dumping an arbitrarily large log.
                    $stream.offset = [Math]::Max(0L, $file.Length - 65536L)
                    $skipPartial = $stream.offset -gt 0
                    $stream.initial = $false
                }
                [void]$file.Seek($stream.offset, [IO.SeekOrigin]::Begin)
                $bytes = New-Object byte[] 65536
                $count = $file.Read($bytes, 0, $bytes.Length)
                $stream.offset += $count
                if ($count -gt 0) {
                    $chars = New-Object char[] 65537
                    $charCount = $stream.decoder.GetChars($bytes, 0, $count, $chars, 0, $false)
                    $text = $stream.pending + [string]::new($chars, 0, $charCount)
                    $lines = $text -split "`n", 0, 'SimpleMatch'
                    $stream.pending = $lines[-1]
                    $start = if ($skipPartial) { 1 } else { 0 }
                    for ($i = $start; $i -lt $lines.Length - 1; $i++) {
                        $line = $lines[$i].TrimEnd("`r")
                        foreach ($secret in $secrets) { if ($secret) { $line = $line.Replace($secret, '<redacted>') } }
                        Write-Output $line
                    }
                }
            } catch [IO.IOException] {
                # Files may disappear briefly while supervision is restarting.
            } finally { if ($file) { $file.Dispose() } }
        }
        # Runtime telemetry is authoritative; old log text is not proof that the
        # current process is connected. Never let stale PID/nonce claim success.
        $status = Read-ConsoleJson '.portal-connection-status.json'
        if ($status -and $runtime -and $status.pid -eq $runtime.pid -and $status.nonce -eq $runtime.nonce) {
            $connection = [string]$status.state
        }
        $display = if (-not $alive) { 'stopped' } elseif ($launch -and -not $link) { 'local' } else { $connection }
        if ($display -ne $shownConnection) {
            Show-ConnectionStatus $display $launch
            $shownConnection = $display
        }
        Start-Sleep -Milliseconds 300
    }
} finally {
    # No shutdown request: closing this reader must not stop Portal or its guardian.
}
