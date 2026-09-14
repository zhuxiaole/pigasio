@echo off
REM ---------------------------------------------------------------------------
REM Build and package PigASIO into the dist\ folder.
REM
REM This file is deliberately ASCII-only: cmd.exe parses .bat files using the
REM system ANSI codepage (GBK on a Chinese Windows), so UTF-8 Chinese text in
REM here would be mis-decoded and could be executed as bogus commands.
REM
REM All the localized output lives in build.ps1, which PowerShell reads as
REM UTF-8 thanks to its byte-order mark.
REM ---------------------------------------------------------------------------

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1"

echo.
pause
