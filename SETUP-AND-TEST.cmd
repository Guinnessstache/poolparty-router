@echo off
rem Finds a model and llama-server on this PC, builds and starts the router,
rem runs every check, and opens the dashboard.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\setup-and-test.ps1" %*
pause
