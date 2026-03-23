#!/usr/bin/env bash
set -euo pipefail

health_port="${1:-4201}"
domain="gui/$(id -u)"

print_launchd_state() {
  local label="$1"
  launchctl print "$domain/$label" 2>/dev/null | rg 'state =|pid =|last exit code =|runs =' || true
}

detect_launchd_label() {
  local label
  for label in dev.codexclaw.agent com.codexclaw.agent; do
    if launchctl print "$domain/$label" >/dev/null 2>&1; then
      printf '%s\n' "$label"
      return 0
    fi
  done
  return 1
}

detect_port_pid() {
  lsof -nP -iTCP:"$health_port" -sTCP:LISTEN 2>/dev/null | awk 'NR==2 {print $2}'
}

label="$(detect_launchd_label || true)"
pid="$(detect_port_pid || true)"

if [[ -n "${label}" ]]; then
  echo "manager=launchd"
  echo "label=${label}"
  print_launchd_state "$label"
fi

if [[ -n "${pid}" ]]; then
  echo "port_pid=${pid}"
  ps -p "$pid" -o pid=,lstart=,etime=,command=
fi

if [[ -z "${label}" && -z "${pid}" ]]; then
  echo "manager=unknown"
  exit 1
fi
