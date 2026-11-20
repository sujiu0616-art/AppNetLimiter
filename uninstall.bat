@echo off
rem ============================================================
rem  AppNetLimiter - uninstall helper
rem  Run this file as Administrator (right click -> Run as admin)
rem  It stops the app, unloads the WinDivert driver service,
rem  after that you can delete this folder safely.
rem ============================================================
setlocal

net session >nul 2>&1
if errorlevel 1 (
    echo [!] Please run as Administrator.
    pause
    exit /b 1
)

echo [1/3] stopping AppNetLimiter.exe ...
taskkill /f /im AppNetLimiter.exe >nul 2>&1
timeout /t 2 /nobreak >nul

echo [2/3] stopping WinDivert driver ...
sc stop WinDivert >nul 2>&1
timeout /t 2 /nobreak >nul

echo [3/3] deleting WinDivert service ...
sc delete WinDivert >nul 2>&1

echo.
echo Done. Now you can delete this folder.
echo If the service is still listed, reboot once and it will be gone.
pause
