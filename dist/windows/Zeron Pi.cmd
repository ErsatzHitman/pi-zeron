@echo off
rem Zeron for Pi: keep its own data and port so it runs beside a stock Zeron.
set "ZERON_DATA_DIR=%LOCALAPPDATA%\ZeronPi"
set "ZERON_IPC_PORT=27754"
start "" "%~dp0zeron.exe" %*
