#!/bin/bash
# Removes what setup-mac.sh installed. The GPU limit returns to default at next boot.
AGENT="$HOME/Library/LaunchAgents/com.poolparty.llama-server.plist"
DAEMON=/Library/LaunchDaemons/com.poolparty.gpu-wired-limit.plist
launchctl bootout "gui/$(id -u)" "$AGENT" 2>/dev/null; rm -f "$AGENT"
sudo launchctl bootout system "$DAEMON" 2>/dev/null; sudo rm -f "$DAEMON"
echo "Removed. ~/.poolparty-llama-key was kept; delete it if you no longer need it."
