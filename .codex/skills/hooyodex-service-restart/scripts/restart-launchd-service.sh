#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/restart-lock.sh"

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <label> <plist-path>" >&2
  exit 2
fi

label="$1"
plist_path="$2"
domain="gui/$(id -u)"
current_pid="$(launchctl print "$domain/$label" 2>/dev/null | sed -n 's/^[[:space:]]*pid = //p' | head -n1)"

if restart_lock::clear_existing_if_restarted "${current_pid:-}"; then
  echo "restart already observed via existing lock; not issuing another launchd restart"
  exit 0
fi

restart_lock::create "${current_pid:-}"

echo "before:"
launchctl print "$domain/$label" 2>/dev/null | rg 'state =|pid =|last exit code =|runs =' || true

launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
launchctl bootout "$domain" "$plist_path" >/dev/null 2>&1 || true
launchctl bootstrap "$domain" "$plist_path"
launchctl kickstart -k "$domain/$label"

echo "after:"
launchctl print "$domain/$label" | rg 'state =|pid =|last exit code =|runs ='
