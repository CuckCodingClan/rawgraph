@echo off
setlocal
rem Build and run the current RawGraph source checkout on Windows.
rem Double-click this file. The first build takes a few minutes; later runs rebuild only changes.
cd /d "%~dp0"
rem Build output stays on this PC's local disk: compiling over the network share is much slower.
if not defined RAWGRAPH_TARGET set "RAWGRAPH_TARGET=%LOCALAPPDATA%\rawgraph-target"
set "CARGO_TARGET_DIR=%RAWGRAPH_TARGET%"
where cargo >nul 2>nul
if errorlevel 1 (
  echo Cargo was not found. Install Rust from https://rustup.rs and try again.
  pause
  exit /b 1
)
echo Building RawGraph from %CD%
echo Build output: %CARGO_TARGET_DIR%
echo.
rem "iter" = optimised like release but without link-time optimisation, so rebuilds take seconds.
cargo run --locked --profile iter -p rawgraph-app
if errorlevel 1 (
  echo.
  echo RawGraph did not start. The first error above explains why.
  pause
  exit /b 1
)
