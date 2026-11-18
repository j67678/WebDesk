@echo off
setlocal enabledelayedexpansion

rem ============================================================
rem  WebDesk dev launcher (debug build)
rem
rem  Usage: run.bat                 GUI mode (starts server automatically)
rem         run.bat headless        headless mode (Ctrl+C to quit)
rem         run.bat minimized       start minimized to tray
rem         run.bat --port 9000     any other args are passed through
rem
rem  Note: the main program is a Windows GUI subsystem binary. When started
rem        from this console it attaches to the parent console, so the log
rem        output is still visible.
rem ============================================================

cd /d "%~dp0"

rem ---- Make sure cargo is available -----------------------------------
where cargo >nul 2>nul
if errorlevel 1 (
    if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
        set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
    )
)
where cargo >nul 2>nul
if errorlevel 1 (
    echo [ERROR] cargo not found. Install Rust first: https://rustup.rs
    pause
    exit /b 1
)

rem ---- Shorthand first arg: headless / minimized ----------------------
set "ARGS="
if /i "%~1"=="headless" (
    set "ARGS=--headless"
    shift
) else if /i "%~1"=="minimized" (
    set "ARGS=--minimized"
    shift
)

rem ---- Append remaining args ------------------------------------------
:collect
if "%~1"=="" goto :start
set ARGS=%ARGS% "%~1"
shift
goto :collect

:start
echo Building and starting WebDesk (debug) ...
echo.

cargo run -- %ARGS%
set "CODE=%ERRORLEVEL%"

if not "%CODE%"=="0" (
    echo.
    echo [FAILED] exit code %CODE%
    pause
)
exit /b %CODE%
