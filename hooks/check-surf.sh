#!/bin/sh
# Silent when surf is installed; otherwise tells the agent how to install it (with the user's consent).
command -v surf >/dev/null 2>&1 && exit 0
if [ -x "$HOME/.local/bin/surf" ]; then
    echo "surf plugin: surf is installed at ~/.local/bin/surf but not on PATH. Call it by that full path, and tell the user to add ~/.local/bin to PATH."
    exit 0
fi
echo "surf plugin: the surf CLI is not installed. Before using the surf skill, ask the user to approve installing it with: curl -fsSL https://raw.githubusercontent.com/juancarlosbs/surf/main/scripts/install.sh | sh"
