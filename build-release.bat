@echo off
setlocal enabledelayedexpansion

rem ============================================================
rem  WebDesk release build script
rem  Usage: build-release.bat          build and package
rem         build-release.bat clean    clean target, then build
rem  Output: dist\webdesk-<version>-windows-<arch>\
rem          dist\webdesk-<version>-windows-<arch>.zip
rem ============================================================

cd /d "%~dp0"

set "APP_NAME=webdesk"

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
    goto :failed
)

rem ---- Read version from Cargo.toml -----------------------------------
set "VERSION="
for /f "tokens=3 delims= " %%v in ('findstr /b /c:"version" Cargo.toml') do (
    if not defined VERSION set "VERSION=%%~v"
)
if not defined VERSION (
    echo [ERROR] Cannot read version from Cargo.toml
    goto :failed
)

rem ---- Target architecture --------------------------------------------
set "ARCH=x64"
if /i "%PROCESSOR_ARCHITECTURE%"=="ARM64" set "ARCH=arm64"
if /i "%PROCESSOR_ARCHITECTURE%"=="x86" (
    if /i "%PROCESSOR_ARCHITEW6432%"=="AMD64" set "ARCH=x64"
)

set "DIST=%CD%\dist"
set "PKG_NAME=%APP_NAME%-%VERSION%-windows-%ARCH%"
set "PKG_DIR=%DIST%\%PKG_NAME%"
set "ZIP_FILE=%DIST%\%PKG_NAME%.zip"

if /i "%~1"=="clean" (
    echo [0/3] Cleaning target and dist ...
    cargo clean
    if exist "%DIST%" rmdir /s /q "%DIST%"
)

rem ---- 1. Build release ------------------------------------------------
echo.
echo [1/3] Building release (v%VERSION%, %ARCH%) ...
cargo build --release
if errorlevel 1 goto :failed

if not exist "target\release\%APP_NAME%.exe" (
    echo [ERROR] Missing artifact: target\release\%APP_NAME%.exe
    goto :failed
)

rem ---- 2. Stage release directory --------------------------------------
echo.
echo [2/3] Staging dist\%PKG_NAME% ...
if exist "%PKG_DIR%" rmdir /s /q "%PKG_DIR%"
mkdir "%PKG_DIR%"
if errorlevel 1 goto :failed

copy /y "target\release\%APP_NAME%.exe" "%PKG_DIR%\" >nul
if errorlevel 1 goto :failed
if exist "config.ini" copy /y "config.ini" "%PKG_DIR%\" >nul
if exist "README.md" copy /y "README.md" "%PKG_DIR%\" >nul

rem ---- 3. Package zip --------------------------------------------------
echo.
echo [3/3] Packaging dist\%PKG_NAME%.zip ...
if exist "%ZIP_FILE%" del /q "%ZIP_FILE%"
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
    "Compress-Archive -Path '%PKG_DIR%' -DestinationPath '%ZIP_FILE%' -CompressionLevel Optimal"
if errorlevel 1 (
    echo [ERROR] Packaging failed
    goto :failed
)

echo.
echo ============================================================
echo  Build complete
echo  Executable: dist\%PKG_NAME%\%APP_NAME%.exe
for %%f in ("%ZIP_FILE%") do echo  Archive:    dist\%%~nxf  (%%~zf bytes)
echo ============================================================
if "%~1"=="" pause
exit /b 0

:failed
echo.
echo [FAILED] Build did not complete. Check the output above.
if "%~1"=="" pause
exit /b 1
