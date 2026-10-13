@echo off
rem Builds (first time only) and starts the PoolParty router with router.toml.
cd /d "%~dp0"
if not exist target\release\poolparty-router.exe (
  echo Building the router, this takes a minute the first time...
  cargo build --release || exit /b 1
)
target\release\poolparty-router.exe router.toml
pause
