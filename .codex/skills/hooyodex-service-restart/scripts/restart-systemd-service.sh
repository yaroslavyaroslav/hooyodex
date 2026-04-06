#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/restart-lock.sh"

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <unit-name>" >&2
  exit 2
fi

unit="$1"
current_pid="$(systemctl --user show "$unit" --property MainPID --value 2>/dev/null || true)"

if restart_lock::clear_existing_if_restarted "${current_pid:-}"; then
  echo "restart already observed via existing lock; not issuing another systemd restart"
  exit 0
fi

restart_lock::create "${current_pid:-}"

echo "before:"
systemctl --user status --no-pager "$unit" || true

systemctl --user daemon-reload
systemctl --user restart "$unit"

echo "after:"
systemctl --user status --no-pager "$unit"
