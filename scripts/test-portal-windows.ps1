# Run with Windows PowerShell 5.1 or PowerShell 7. All OS task/process calls
# below are mocked; this test never installs a task or stops a real process.
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'portal-task-common.ps1')

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

foreach ($link in @('', 'https://example.test/', 'https://example.test/being/',
        'https://example.test/being/?token=', 'https://example.test/being/?other=token=x',
        'file:///being/?token=x')) {
    $rejected = $false
    try { $null = Get-PortalLoomUri $link } catch { $rejected = $true }
    Assert-True $rejected "Invalid Loom link was accepted: $link"
}
$valid = Get-PortalLoomUri 'https://example.test/being/?token=test-token&other=value'
Assert-True ($valid.Host -eq 'example.test') 'Valid Loom link rejected.'
Assert-True (Test-PortalScriptCommand 'powershell -File "C:\Portal Root\scripts\portal-supervisor.ps1"' 'C:\Portal Root\scripts\portal-supervisor.ps1') 'Quoted script path must match.'
Assert-True (-not (Test-PortalScriptCommand 'powershell -File C:\portal-other\scripts\portal-supervisor.ps1' 'C:\portal\scripts\portal-supervisor.ps1')) 'Another checkout must not match.'

$global:PortalInstallerTestEvents = [System.Collections.Generic.List[string]]::new()
$global:PortalInstallerTestFailRegistration = $true
$global:PortalInstallerTestForeignTask = $false
function Get-ScheduledTask {
    if ($global:PortalInstallerTestForeignTask) {
        return [pscustomobject]@{ Actions = @([pscustomobject]@{ Arguments = 'unrelated-script.ps1' }) }
    }
}
function New-ScheduledTaskAction { return [pscustomobject]@{} }
function New-ScheduledTaskTrigger { return [pscustomobject]@{} }
function New-ScheduledTaskPrincipal { return [pscustomobject]@{} }
function New-ScheduledTaskSettingsSet { return [pscustomobject]@{} }
function Register-ScheduledTask {
    $global:PortalInstallerTestEvents.Add('register')
    if ($global:PortalInstallerTestFailRegistration) { throw 'Simulated registration failure' }
}
function Stop-ScheduledTask { $global:PortalInstallerTestEvents.Add('stop') }
function Unregister-ScheduledTask { $global:PortalInstallerTestEvents.Add('unregister') }
function Get-CimInstance { $global:PortalInstallerTestEvents.Add('query-processes') }
function Start-ScheduledTask { $global:PortalInstallerTestEvents.Add('start') }

$tempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = Join-Path $tempBase ('portal-installer-test-' + [guid]::NewGuid().ToString())
try {
    New-Item -ItemType Directory -Path (Join-Path $testRoot 'target\release') -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $testRoot 'scripts') -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $testRoot 'target\release\heart-portal.exe') -Value 'test fixture'
    Set-Content -LiteralPath (Join-Path $testRoot 'portal.toml') -Value 'name = "test"'
    foreach ($scriptName in @('portal-supervisor.ps1', 'portal-supervisor-hidden.vbs')) {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $scriptName) -Destination (Join-Path $testRoot "scripts\$scriptName")
    }
    $linkFile = Join-Path $testRoot '.portal-connection.url'
    $oldLink = 'https://example.test/being/?token=old-test-token'
    $newLink = 'https://example.test/being/?token=new-test-token'
    Set-Content -LiteralPath $linkFile -Value $oldLink -NoNewline
    $installer = Join-Path $PSScriptRoot 'install-portal-windows.ps1'

    $failed = $false
    try { & $installer -Root $testRoot -PortalName test-machine -ConnectLink $newLink }
    catch {
        $failed = $_.Exception.Message -eq 'Simulated registration failure'
        if (-not $failed) { throw }
    }
    Assert-True $failed 'Expected registration failure.'
    Assert-True ((Get-Content -LiteralPath $linkFile -Raw) -eq $oldLink) 'Failed registration replaced working credentials.'
    Assert-True (($global:PortalInstallerTestEvents -join ',') -eq 'register') 'Failed registration stopped existing processes.'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $testRoot '.portal-name'))) 'Failed registration saved a new identity.'

    $global:PortalInstallerTestEvents.Clear()
    $global:PortalInstallerTestFailRegistration = $false
    $global:PortalInstallerTestForeignTask = $true
    $failed = $false
    try { & $installer -Root $testRoot -PortalName test-machine -ConnectLink $newLink }
    catch { $failed = $_.Exception.Message -like '*does not belong to this checkout*' }
    Assert-True $failed 'Installer accepted a task owned by another checkout.'
    Assert-True ($global:PortalInstallerTestEvents.Count -eq 0) 'Ownership failure changed task/process state.'
    Assert-True ((Get-Content -LiteralPath $linkFile -Raw) -eq $oldLink) 'Ownership failure replaced credentials.'

    $global:PortalInstallerTestForeignTask = $false
    & $installer -Root $testRoot -PortalName test-machine -ConnectLink $newLink
    Assert-True ((Get-Content -LiteralPath $linkFile -Raw) -eq $newLink) 'Successful install did not save credentials.'
    Assert-True ((Get-PortalSavedValue $testRoot '.portal-name') -eq 'test-machine') 'Portal identity was not saved.'
    Assert-True ((Get-PortalSavedValue $testRoot '.portal-task-name') -eq 'HeartPortal-test-machine') 'Task name was not saved.'
    Assert-True (($global:PortalInstallerTestEvents -join ',') -eq 'register,stop,query-processes,query-processes,start') 'Unexpected install order.'
    Write-Output 'Windows installer regression checks passed.'
} finally {
    $resolvedTestRoot = [IO.Path]::GetFullPath($testRoot)
    if (-not $resolvedTestRoot.StartsWith($tempBase, [StringComparison]::OrdinalIgnoreCase) -or
        (Split-Path $resolvedTestRoot -Leaf) -notlike 'portal-installer-test-*') {
        throw 'Refusing to clean up a path outside the temporary test directory.'
    }
    if (Test-Path -LiteralPath $resolvedTestRoot) { Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force }
    Remove-Variable -Name PortalInstallerTestEvents,PortalInstallerTestFailRegistration,PortalInstallerTestForeignTask -Scope Global -ErrorAction SilentlyContinue
}
