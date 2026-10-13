@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\setup-and-test.ps1" -Stop
pause
