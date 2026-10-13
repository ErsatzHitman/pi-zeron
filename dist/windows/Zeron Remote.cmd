@echo off
rem Zeron on this PC, its engine (and Pi) on another machine.
rem
rem On the other machine (Linux or macOS): install Zeron, put PI_EXECUTABLE and
rem the model API keys in ~/.zeron/env, run `zeron daemon install`.
rem Here: run `Zeron Remote.cmd user@host` (or set ZERON_REMOTE once). It
rem forwards a local port to the engine over SSH and opens a window that only
rem views that engine, with data of its own beside the local Zeron.
setlocal
if not "%~1"=="" set "ZERON_REMOTE=%~1"
if "%ZERON_REMOTE%"=="" (
  echo Usage: "Zeron Remote.cmd" user@host
  exit /b 1
)
if "%ZERON_REMOTE_PORT%"=="" set "ZERON_REMOTE_PORT=27755"
start "Zeron tunnel to %ZERON_REMOTE%" /min ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=3 -L %ZERON_REMOTE_PORT%:127.0.0.1:27654 %ZERON_REMOTE%
rem Give the tunnel a moment to listen before the window probes it.
timeout /t 3 /nobreak >nul
set "ZERON_IPC_PORT=%ZERON_REMOTE_PORT%"
set "ZERON_DATA_DIR=%LOCALAPPDATA%\ZeronRemote"
set "ZERON_ATTACH_ONLY=1"
start "" "%~dp0zeron.exe"
