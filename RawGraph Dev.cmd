@echo off
setlocal
rem Build and run the current RawGraph source checkout on Windows.
rem Double-click this file. The first build is slow; later runs rebuild only changes.
cd /d "%~dp0"
set "CARGO_TARGET_DIR=%~dp0..\rawgraph-target-windows"
where cargo >nul 2>nul
if errorlevel 1 (
  echo Cargo was not found. Install Rust from https://rustup.rs and try again.
  pause
  exit /b 1
)
echo Building RawGraph from %CD%
echo Build output: %CARGO_TARGET_DIR%
echo.
cargo run --locked --release -p rawgraph-app
if errorlevel 1 (
  echo.
  echo RawGraph did not start. The first error above explains why.
  pause
  exit /b 1
)
