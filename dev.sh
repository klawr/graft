#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEVCONTAINER="$SCRIPT_DIR/.devcontainer/devcontainer.json"

workspace_name="$(basename "$SCRIPT_DIR")"

if command -v jq &>/dev/null && [[ -f "$DEVCONTAINER" ]]; then
  session_name="$(jq -r '.name // empty' "$DEVCONTAINER")"
fi

session_name="${session_name:-graft-${workspace_name}}"

if tmux has-session -t "$session_name" 2>/dev/null; then
  tmux attach-session -t "$session_name"
else
  tmux new-session -s "$session_name" -c "$SCRIPT_DIR"
fi
