@echo off
rem Entry point the recipient double-clicks (Swedish messages live in Install.ps1).
rem
rem A .cmd rather than the .ps1 directly, for two reasons: Windows does not run a
rem .ps1 on double-click (it opens in Notepad), and the machine's ExecutionPolicy
rem may refuse the script. -ExecutionPolicy Bypass applies to this run only.
rem This file stays ASCII: cmd.exe does not read UTF-8 reliably.
setlocal
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0Install.ps1"
if errorlevel 1 (
  echo.
  echo   Installationen misslyckades. Se meddelandet ovan.
  rem pause so the window stays open - a window that closes on failure takes the explanation with it.
  pause
  endlocal
  exit /b 1
)
endlocal
