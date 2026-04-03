#!/usr/bin/env bash
set -euo pipefail

SERVICE_PATH_DEFAULT="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin"
LOG_DIR="$HOME/Library/Logs"
CONFIG_PATH_DEFAULT="$HOME/Library/Application Support/hooyodex/config.toml"
ALT_CONFIG_PATH="$HOME/.config/hooyodex/config.toml"
DEV_LABEL="dev.hooyodex.agent"
COM_LABEL="com.hooyodex.agent"
DEV_PLIST="$HOME/Library/LaunchAgents/$DEV_LABEL.plist"
COM_PLIST="$HOME/Library/LaunchAgents/$COM_LABEL.plist"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SKILL_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  restart_hooyodex_service.sh status
  restart_hooyodex_service.sh restart
  restart_hooyodex_service.sh repair-plist <dev.hooyodex.agent|com.hooyodex.agent> [binary_path] [config_path]

This script is sanitized for reusable skill guidance:
- uses $HOME-based defaults
- prints active label, PID, and health
- can rewrite a missing/broken plist from bundled templates
EOF
}

launchctl_summary() {
  local label="$1"
  launchctl print "gui/$(id -u)/$label" 2>/dev/null | rg 'state =|pid =|last exit code =|runs =' || true
}

choose_active_label() {
  local out
  out="$(launchctl_summary "$DEV_LABEL")"
  if [[ -n "$out" ]]; then
    printf '%s\n' "$DEV_LABEL"
    return 0
  fi

  out="$(launchctl_summary "$COM_LABEL")"
  if [[ -n "$out" ]]; then
    printf '%s\n' "$COM_LABEL"
    return 0
  fi

  if lsof -nP -iTCP:4201 -sTCP:LISTEN 2>/dev/null | rg -q 'hooyodex'; then
    local pid
    pid="$(lsof -nP -iTCP:4201 -sTCP:LISTEN -t 2>/dev/null | head -n1)"
    if ps -p "$pid" -o command= | rg -q 'target/release/hooyodex'; then
      printf '%s\n' "$COM_LABEL"
      return 0
    fi
    if ps -p "$pid" -o command= | rg -q 'target/debug/hooyodex'; then
      printf '%s\n' "$DEV_LABEL"
      return 0
    fi
  fi

  printf '%s\n' "$DEV_LABEL"
}

choose_binary() {
  local label="$1"
  if [[ "$label" == "$COM_LABEL" && -x "$PWD/target/release/hooyodex" ]]; then
    printf '%s\n' "$PWD/target/release/hooyodex"
    return 0
  fi
  if [[ "$label" == "$DEV_LABEL" && -x "$PWD/target/debug/hooyodex" ]]; then
    printf '%s\n' "$PWD/target/debug/hooyodex"
    return 0
  fi
  if [[ -x "$PWD/target/release/hooyodex" ]]; then
    printf '%s\n' "$PWD/target/release/hooyodex"
    return 0
  fi
  if [[ -x "$PWD/target/debug/hooyodex" ]]; then
    printf '%s\n' "$PWD/target/debug/hooyodex"
    return 0
  fi
  return 1
}

choose_config() {
  if [[ -f "$CONFIG_PATH_DEFAULT" ]]; then
    printf '%s\n' "$CONFIG_PATH_DEFAULT"
    return 0
  fi
  if [[ -f "$ALT_CONFIG_PATH" ]]; then
    printf '%s\n' "$ALT_CONFIG_PATH"
    return 0
  fi
  printf '%s\n' "$CONFIG_PATH_DEFAULT"
}

plist_path_for_label() {
  local label="$1"
  if [[ "$label" == "$COM_LABEL" ]]; then
    printf '%s\n' "$COM_PLIST"
  else
    printf '%s\n' "$DEV_PLIST"
  fi
}

template_for_label() {
  local label="$1"
  if [[ "$label" == "$COM_LABEL" ]]; then
    printf '%s\n' "$SKILL_DIR/assets/plists/com.hooyodex.agent.plist.template"
  else
    printf '%s\n' "$SKILL_DIR/assets/plists/dev.hooyodex.agent.plist.template"
  fi
}

escape_sed() {
  printf '%s' "$1" | sed -e 's/[\/&|]/\\&/g'
}

render_plist() {
  local label="$1"
  local bin_path="$2"
  local config_path="$3"
  local plist_path template working_dir
  plist_path="$(plist_path_for_label "$label")"
  template="$(template_for_label "$label")"

  if [[ "$label" == "$COM_LABEL" ]]; then
    working_dir="$(dirname "$config_path")"
  else
    working_dir="$HOME"
  fi

  mkdir -p "$(dirname "$plist_path")" "$LOG_DIR"

  sed \
    -e "s|__HOOYODEX_BIN__|$(escape_sed "$bin_path")|g" \
    -e "s|__HOOYODEX_CONFIG__|$(escape_sed "$config_path")|g" \
    -e "s|__WORKING_DIRECTORY__|$(escape_sed "$working_dir")|g" \
    -e "s|__SERVICE_PATH__|$(escape_sed "$SERVICE_PATH_DEFAULT")|g" \
    -e "s|__STDOUT_PATH__|$(escape_sed "$LOG_DIR/hooyodex.out.log")|g" \
    -e "s|__STDERR_PATH__|$(escape_sed "$LOG_DIR/hooyodex.err.log")|g" \
    "$template" > "$plist_path"

  printf '%s\n' "$plist_path"
}

current_pid_for_label() {
  local label="$1"
  launchctl print "gui/$(id -u)/$label" 2>/dev/null | sed -n 's/^[[:space:]]*pid = //p' | head -n1
}

print_health() {
  curl -fsS http://127.0.0.1:4201/healthz
  echo
  curl -fsS http://127.0.0.1:4201/readyz
  echo
}

restart_label() {
  local label="$1"
  local old_pid new_pid
  old_pid="$(current_pid_for_label "$label" || true)"
  if [[ -n "$old_pid" ]]; then
    ps -p "$old_pid" -o pid,lstart,etime,command
  fi

  launchctl kickstart -k "gui/$(id -u)/$label"
  sleep 1

  launchctl_summary "$label"
  new_pid="$(current_pid_for_label "$label" || true)"
  if [[ -n "$new_pid" ]]; then
    ps -p "$new_pid" -o pid,lstart,etime,command
  fi

  if [[ -n "$old_pid" && -n "$new_pid" && "$old_pid" == "$new_pid" ]]; then
    echo "PID did not change after restart" >&2
    return 1
  fi

  print_health
}

repair_plist() {
  local label="$1"
  local bin_path="${2:-}"
  local config_path="${3:-}"
  if [[ -z "$bin_path" ]]; then
    bin_path="$(choose_binary "$label")"
  fi
  if [[ -z "$config_path" ]]; then
    config_path="$(choose_config)"
  fi
  render_plist "$label" "$bin_path" "$config_path"
}

cmd="${1:-}"
case "$cmd" in
  status)
    label="$(choose_active_label)"
    echo "label=$label"
    launchctl_summary "$label"
    pid="$(current_pid_for_label "$label" || true)"
    if [[ -n "${pid:-}" ]]; then
      ps -p "$pid" -o pid,lstart,etime,command
    fi
    print_health
    ;;
  restart)
    label="$(choose_active_label)"
    echo "label=$label"
    restart_label "$label"
    ;;
  repair-plist)
    if [[ $# -lt 2 ]]; then
      usage >&2
      exit 1
    fi
    repair_plist "$2" "${3:-}" "${4:-}"
    ;;
  *)
    usage >&2
    exit 1
    ;;
esac
