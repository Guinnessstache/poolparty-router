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

# llama-server runs in the background from launchd, which doesn't get the
# Files & Folders permission Terminal has. Models in these folders can't be
# opened from there ("Operation not permitted") and the server never starts.
case "$MODEL" in
  /Volumes/*)
    echo "Note: the model is on an external drive. If the server doesn't start,"
    echo "macOS may be blocking background access to it; copy it to ~/models instead."
    ;;
  "$HOME/Desktop/"*|"$HOME/Documents/"*|"$HOME/Downloads/"*|"$HOME/Library/Mobile Documents/"*)
    echo "The model is in $(dirname "$MODEL"), which background apps on macOS"
    echo "aren't allowed to read. Move it somewhere like ~/models and run this again:"
    echo
    echo "  mkdir -p ~/models && mv \"$(dirname "$MODEL")\"/$(basename "${MODEL%-00001-of-*}")* ~/models/"
    echo "  bash \"$0\" ~/models/$(basename "$MODEL")"
    exit 1
    ;;
esac

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
DOMAIN="gui/$(id -u)"
SERVICE="$DOMAIN/com.poolparty.llama-server"
LOG="$HOME/Library/Logs/poolparty-llama-server.log"
# bootout returns before the old copy is gone; bootstrapping too soon fails
# with "Bootstrap failed: 5: Input/output error" when re-running this script.
launchctl bootout "$SERVICE" 2>/dev/null || true
for _ in $(seq 1 30); do launchctl print "$SERVICE" >/dev/null 2>&1 || break; sleep 1; done

HOLDER="$(lsof -nP -iTCP:"$PORT" -sTCP:LISTEN 2>/dev/null | awk 'NR==2 {print $1" (pid "$2")"}' || true)"
if [[ -n "$HOLDER" ]]; then
  echo "Port $PORT is already in use by $HOLDER. Quit it (or run with PORT=8081) and try again."
  exit 1
fi

: > "$LOG"   # fresh log, so what's shown below is from this start
launchctl bootstrap "$DOMAIN" "$AGENT"

# Wait for the model to load (large models can take a few minutes) and say
# plainly whether it worked, instead of leaving it to the log.
echo
echo "Starting llama-server and loading the model..."
STARTED=0
for i in $(seq 1 120); do                      # up to 6 minutes
  CODE="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/health" || true)"
  if [[ "$CODE" == "200" ]]; then STARTED=1; break; fi
  if ! pgrep -qf "$LLAMA.*--port $PORT"; then
    sleep 2
    pgrep -qf "$LLAMA.*--port $PORT" || break   # exited; launchd retries in 60 s
  fi
  (( i % 10 == 0 )) && echo "  still loading ($((i * 3)) s)..."
  sleep 3
done

if [[ "$STARTED" != 1 ]]; then
  echo
  echo "llama-server did not come up. Last lines of its log:"
  echo "------------------------------------------------------------"
  tail -n 25 "$LOG" 2>/dev/null || echo "(no log written: $LOG)"
  echo "------------------------------------------------------------"
  if grep -q "Operation not permitted" "$LOG" 2>/dev/null; then
    echo "macOS blocked access to the model file. Move it to ~/models and run this again."
  elif grep -qiE "unknown model architecture|failed to load model|error loading model" "$LOG" 2>/dev/null; then
    echo "This llama.cpp can't load that model. Try:  brew upgrade llama.cpp  then run this again."
  elif grep -qiE "out of memory|failed to allocate|insufficient memory" "$LOG" 2>/dev/null; then
    echo "Not enough GPU memory. Try a smaller context:  CTX=32768 bash \"$0\" \"$MODEL\""
  elif grep -qiE "address already in use|couldn't bind" "$LOG" 2>/dev/null; then
    echo "Port $PORT is taken. Run with another port:  PORT=8081 bash \"$0\" \"$MODEL\""
  fi
  echo "launchd will keep retrying every minute. Full log: tail -f $LOG"
  exit 1
fi

echo "llama-server is running."
echo "Log:   tail -f $LOG"
echo "Check: curl -s -H 'Authorization: Bearer $KEY' http://localhost:$PORT/v1/models"
echo
echo "Now, in PoolParty Router on your PC: Mac Studio -> Set up, then enter"
echo
echo "  Address:  $(scutil --get LocalHostName 2>/dev/null || hostname -s).local:$PORT   (or the Mac's Tailscale name/IP)"
echo "  Password: $KEY"
echo
echo "and press Test, then Save."
