#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <unit-name>" >&2
  exit 2
fi

unit="$1"

echo "before:"
systemctl --user status --no-pager "$unit" || true

systemctl --user daemon-reload
systemctl --user restart "$unit"

echo "after:"
systemctl --user status --no-pager "$unit"
