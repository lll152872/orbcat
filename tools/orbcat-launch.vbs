' orbcat-launch.vbs -- console-free host for the desktop shortcut.
'
' WHY THIS FILE EXISTS
'   A desktop .lnk cannot point straight at a .ps1: Windows has no ShellExecute
'   association for .ps1, so double-clicking pops up "How do you want to open
'   this file?". Pointing it at pwsh.exe instead makes a console window FLASH on
'   screen -- even with -WindowStyle Hidden, the console is created first and
'   hidden afterwards.
'
'   wscript.exe is a GUI-subsystem program with no console of its own. It starts
'   pwsh through WScript.Shell.Run(cmd, 0, False): window style 0 = hidden, so
'   the console never appears on screen at all.
'
' CALL CHAIN (every path in it is fixed)
'   desktop .lnk
'     -> wscript.exe   (system directory, constant)
'     -> this file     (<repo>\tools\, changes only if the repo moves)
'     -> pwsh, hidden
'     -> orbcat-launch.ps1   <- decides "which build works" at each double-click
'     -> the chosen orbcat.exe
'
'   Renaming build outputs, switching profiles (release / release-fast) or
'   leaving a broken build behind therefore cannot break the desktop icon.
'   If the repo moves, re-run tools\install-shortcut.ps1 once.
'
' NOTE: keep this file pure ASCII. Windows script hosts read .vbs as ANSI, so
'   non-ASCII bytes here corrupt string literals and the script stops parsing.
'   The Chinese explanations live in orbcat-launch.ps1 and install-shortcut.ps1.
'
' MANUAL TROUBLESHOOTING
'   Passing arguments shows a visible window and waits, so output stays put:
'     cscript //nologo tools\orbcat-launch.vbs -List
Option Explicit

Dim fso, sh, here, ps1, exe, cmd, i, extra, cand, callerOwnsOutFile
Set fso = CreateObject("Scripting.FileSystemObject")
Set sh  = CreateObject("WScript.Shell")

' Directory holding this file = <repo>\tools
here = fso.GetParentFolderName(WScript.ScriptFullName)
ps1  = fso.BuildPath(here, "orbcat-launch.ps1")

If Not fso.FileExists(ps1) Then
  sh.Popup "Launcher script not found:" & vbCrLf & ps1 & vbCrLf & vbCrLf & _
           "The desktop shortcut points at this file (orbcat-launch.vbs)." & vbCrLf & _
           "If the orbcat repo was moved, run this again:" & vbCrLf & _
           "  pwsh -File tools\install-shortcut.ps1", _
           0, "orbcat launch failed", 16
  WScript.Quit 1
End If

' Prefer PowerShell 7 (pwsh); fall back to Windows PowerShell 5.1.
exe = ""
cand = sh.ExpandEnvironmentStrings("%ProgramFiles%\PowerShell\7\pwsh.exe")
If fso.FileExists(cand) Then
  exe = cand
Else
  cand = sh.ExpandEnvironmentStrings("%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe")
  If fso.FileExists(cand) Then exe = cand
End If
If exe = "" Then exe = "pwsh.exe"   ' last resort: whatever PATH offers

cmd = """" & exe & """ -NoProfile -NonInteractive -ExecutionPolicy Bypass" & _
      " -WindowStyle Hidden -File """ & ps1 & """"

If WScript.Arguments.Count > 0 Then
  ' Troubleshooting mode: run the real launcher and print its result here, so
  '   cscript //nologo tools\orbcat-launch.vbs -List
  ' exercises the exact desktop chain and shows the answer inline.
  '
  ' The launcher writes UTF-8 to a temp file (-OutFile) instead of stdout: this
  ' script's strings are ANSI, and WScript.Shell.Exec would decode the child's
  ' UTF-8 bytes as ANSI, turning the Chinese text into mojibake. Reading the file
  ' through ADODB.Stream with Charset="utf-8" avoids that entirely.
  extra = ""
  callerOwnsOutFile = False
  For i = 0 To WScript.Arguments.Count - 1
    If LCase(WScript.Arguments(i)) = "-outfile" Then callerOwnsOutFile = True
    extra = extra & " " & WScript.Arguments(i)
  Next

  Dim tmp, rc, ts, st
  If callerOwnsOutFile Then
    ' Caller chose the destination (install-shortcut.ps1 -Verify does this).
    ' Adding our own -OutFile too would bind the parameter twice and fail.
    rc = sh.Run(cmd & extra, 0, True)
    WScript.Quit rc
  End If

  tmp = fso.BuildPath(fso.GetSpecialFolder(2), "orbcat-launch-out.txt")
  On Error Resume Next
  fso.DeleteFile tmp, True
  On Error GoTo 0

  rc = sh.Run(cmd & " -OutFile """ & tmp & """" & extra, 0, True)

  If fso.FileExists(tmp) Then
    Set st = CreateObject("ADODB.Stream")
    st.Type = 2                ' adTypeText
    st.Charset = "utf-8"
    st.Open
    st.LoadFromFile tmp
    ts = st.ReadText()
    st.Close
    If Len(ts) > 0 Then WScript.StdOut.Write ts
    On Error Resume Next
    fso.DeleteFile tmp, True
    On Error GoTo 0
  End If
  WScript.Quit rc
End If

' Normal mode: hidden, do not wait (orbcat outlives this launcher).
sh.Run cmd, 0, False
