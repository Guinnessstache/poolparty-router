#!/bin/bash
# One-time setup on the Mac Studio. Run from this folder:
#   ./setup-mac.sh /path/to/model.gguf
#
# 1. Installs a LaunchDaemon that raises the GPU memory limit at every boot
#    (macOS gives the GPU ~75% of RAM by default, ~96 GB on 128 GB, which is
#    too little for Qwen3.8-Flash-Next).
# 2. Installs a LaunchAgent that runs llama-server at login and restarts it
#    if it exits.
set -euo pipefail

MODEL="${1:-}"
WIRED_MB="${WIRED_MB:-114688}"          # 112 GB for the GPU, ~16 GB left for macOS
PORT="${PORT:-8080}"
CTX="${CTX:-65536}"
HERE="$(cd "$(dirname "$0")" && pwd)"

if [[ -z "$MODEL" || ! -f "$MODEL" ]]; then
  echo "usage: $0 /path/to/model.gguf   (for split GGUFs, the first part: ...-00001-of-0000N.gguf)"
  exit 1
fi
if [[ "$MODEL" == *['&|<>']* ]]; then
  echo "Please move the model to a path without & | < > characters."
  exit 1
fi
MODEL="$(cd "$(dirname "$MODEL")" && pwd)/$(basename "$MODEL")"   # absolute
LLAMA="$(command -v llama-server || true)"
if [[ -z "$LLAMA" ]]; then
  echo "llama-server not found. Install it with:  brew install llama.cpp"
  exit 1
fi
echo "llama-server: $LLAMA ($("$LLAMA" --version 2>&1 | head -1))"

# A key so only the router can use the model. Reused if it already exists.
KEYFILE="$HOME/.poolparty-llama-key"
if [[ ! -f "$KEYFILE" ]]; then
  LC_ALL=C tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 40 > "$KEYFILE"
  chmod 600 "$KEYFILE"
fi
KEY="$(cat "$KEYFILE")"

# --- 1. GPU memory limit at boot (needs sudo once) ---
DAEMON=/Library/LaunchDaemons/com.poolparty.gpu-wired-limit.plist
sed "s/__WIRED_MB__/$WIRED_MB/" "$HERE/com.poolparty.gpu-wired-limit.plist" | sudo tee "$DAEMON" >/dev/null
sudo chown root:wheel "$DAEMON"
sudo chmod 644 "$DAEMON"
sudo launchctl bootout system "$DAEMON" 2>/dev/null || true
sudo launchctl bootstrap system "$DAEMON"
echo "GPU memory limit: $(sysctl -n iogpu.wired_limit_mb) MB"

# --- 2. llama-server at login ---
AGENT="$HOME/Library/LaunchAgents/com.poolparty.llama-server.plist"
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
sed -e "s|__LLAMA__|$LLAMA|" -e "s|__MODEL__|$MODEL|" -e "s|__PORT__|$PORT|" \
    -e "s|__CTX__|$CTX|" -e "s|__KEY__|$KEY|" -e "s|__HOME__|$HOME|" \
    "$HERE/com.poolparty.llama-server.plist" > "$AGENT"
chmod 600 "$AGENT"   # it contains the API key
launchctl bootout "gui/$(id -u)" "$AGENT" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$AGENT"

echo
echo "llama-server is starting (large models take a minute to load)."
echo "Log:   tail -f ~/Library/Logs/poolparty-llama-server.log"
echo "Check: curl -s -H 'Authorization: Bearer $KEY' http://localhost:$PORT/v1/models"
echo
echo "Put this in router.toml on jayspc:"
echo
echo "[nodes.mac]"
echo "url = \"http://$(scutil --get LocalHostName 2>/dev/null || hostname -s):$PORT/v1\"   # or the Mac's Tailscale name"
echo "api_key = \"$KEY\""
