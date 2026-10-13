@echo off
rem Serves the small completion model on the RTX 4070 Ti for the router.
rem Edit MODEL to point at your GGUF. Keep it small (3-7B, Q4) so Unity and
rem Unreal still have VRAM: a 7B Q4 with 8k context uses about 5-6 GB.

set LLAMA=llama-server
set MODEL=C:\Users\thist\models\qwen-coder-small-Q4_K_M.gguf
set PORT=8081

where %LLAMA% >nul 2>nul || (
  echo llama-server was not found on PATH. Get a Windows CUDA build from github.com/ggml-org/llama.cpp/releases
  echo or set LLAMA= to the full path of llama-server.exe in this file.
  exit /b 1
)
if not exist "%MODEL%" (
  echo Model not found: %MODEL%
  echo Edit MODEL in %~f0
  exit /b 1
)

rem 127.0.0.1 only: the router runs on this PC, so nothing else needs to reach it.
%LLAMA% -m "%MODEL%" -ngl 99 -c 8192 --host 127.0.0.1 --port %PORT% --jinja
