Option Explicit

Dim shell, root, portalName, supervisor, powershell, command, exitCode

If WScript.Arguments.Count < 2 Then
    WScript.Quit 2
End If

root = WScript.Arguments(0)
portalName = WScript.Arguments(1)

Set shell = CreateObject("WScript.Shell")
powershell = shell.ExpandEnvironmentStrings("%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe")
supervisor = root & "\scripts\portal-supervisor.ps1"

command = Quote(powershell) _
    & " -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File " _
    & Quote(supervisor) & " -Root " & Quote(root) & " -PortalName " & Quote(portalName)

' Window style 0 starts PowerShell without creating a visible console. Waiting
' keeps the scheduled task tied to the lifetime of the supervisor process.
exitCode = shell.Run(command, 0, True)
WScript.Quit exitCode

Function Quote(value)
    Quote = Chr(34) & Replace(value, Chr(34), Chr(34) & Chr(34)) & Chr(34)
End Function
