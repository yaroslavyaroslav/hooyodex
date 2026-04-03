#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <label> <plist-path>" >&2
  exit 2
fi

label="$1"
plist_path="$2"
domain="gui/$(id -u)"

echo "before:"
launchctl print "$domain/$label" 2>/dev/null | rg 'state =|pid =|last exit code =|runs =' || true

launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
launchctl bootout "$domain" "$plist_path" >/dev/null 2>&1 || true
launchctl bootstrap "$domain" "$plist_path"
launchctl kickstart -k "$domain/$label"

echo "after:"
launchctl print "$domain/$label" | rg 'state =|pid =|last exit code =|runs ='
