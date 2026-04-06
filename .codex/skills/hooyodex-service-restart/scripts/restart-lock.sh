#!/usr/bin/env bash
set -euo pipefail

restart_lock::resolve_project_root() {
  local script_dir root
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  if root="$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null)"; then
    printf '%s\n' "$root"
    return 0
  fi

  printf '%s\n' "$(cd "$script_dir/../../../.." && pwd)"
}

restart_lock::path() {
  local project_root
  project_root="$(restart_lock::resolve_project_root)"
  printf '%s\n' "${HOOYODEX_RESTART_LOCK_PATH:-$project_root/.hooyodex-service-restart.lock}"
}

restart_lock::field() {
  local key="$1"
  local lock_path
  lock_path="$(restart_lock::path)"
  if [[ ! -f "$lock_path" ]]; then
    return 1
  fi

  sed -n "s/^${key}=//p" "$lock_path" | head -n1
}

restart_lock::print_existing() {
  local lock_path
  lock_path="$(restart_lock::path)"
  if [[ ! -f "$lock_path" ]]; then
    return 1
  fi

  echo "restart lock exists: $lock_path" >&2
  cat "$lock_path" >&2
}

restart_lock::already_restarted() {
  local current_pid="$1"
  local previous_pid=""
  local lock_path
  lock_path="$(restart_lock::path)"
  if [[ ! -f "$lock_path" ]]; then
    return 1
  fi

  previous_pid="$(restart_lock::field pid_before_restart || true)"
  if [[ -z "$previous_pid" || -z "$current_pid" ]]; then
    return 1
  fi

  [[ "$previous_pid" != "$current_pid" ]]
}

restart_lock::clear_existing_if_restarted() {
  local current_pid="$1"
  local lock_path previous_pid
  lock_path="$(restart_lock::path)"
  if [[ ! -f "$lock_path" ]]; then
    return 1
  fi

  previous_pid="$(restart_lock::field pid_before_restart || true)"
  if restart_lock::already_restarted "$current_pid"; then
    echo "existing restart lock indicates the service already restarted (old_pid=${previous_pid:-unknown}, current_pid=$current_pid)" >&2
    rm -f "$lock_path"
    echo "cleared restart lock: $lock_path" >&2
    return 0
  fi

  echo "existing restart lock does not prove a restart yet (old_pid=${previous_pid:-unknown}, current_pid=${current_pid:-unknown})" >&2
  rm -f "$lock_path"
  echo "cleared stale restart lock: $lock_path" >&2
  return 1
}

restart_lock::create() {
  local pid_before_restart="${1:-}"
  local lock_path project_root
  lock_path="$(restart_lock::path)"
  project_root="$(restart_lock::resolve_project_root)"
  mkdir -p "$(dirname "$lock_path")"

  cat >"$lock_path" <<EOF
created_at_utc=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
project_root=$project_root
script=${0##*/}
cwd=$PWD
user=${USER:-unknown}
host=$(hostname)
pid_before_restart=$pid_before_restart
EOF

  echo "created restart lock: $lock_path" >&2
}

restart_lock::clear() {
  local lock_path
  lock_path="$(restart_lock::path)"
  rm -f "$lock_path"
  echo "cleared restart lock: $lock_path"
}

restart_lock::status_note() {
  local lock_path
  lock_path="$(restart_lock::path)"
  if [[ -f "$lock_path" ]]; then
    echo "restart_lock=$lock_path"
  else
    echo "restart_lock=absent"
  fi
}
