#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
LABEL_MAC="com.codexclaw.agent"
LABEL_LINUX="codexclaw.service"
CONFIG_DIR="${CODEXCLAW_CONFIG_DIR:-$HOME/.config/codexclaw}"

escape_sed() {
  printf '%s' "$1" | sed -e 's/[\/&|]/\\&/g'
}

resolve_binary() {
  if [[ -n "${CODEXCLAW_BIN:-}" ]]; then
    printf '%s' "$CODEXCLAW_BIN"
    return 0
  fi

  local candidate
  for candidate in \
    "$ROOT_DIR/target/release/codexclaw" \
    "$ROOT_DIR/target/debug/codexclaw" \
    "$(command -v codexclaw 2>/dev/null || true)"
  do
    if [[ -n "$candidate" && -x "$candidate" ]]; then
      printf '%s' "$candidate"
      return 0
    fi
  done

  return 1
}

render_template() {
  local template="$1"
  local dest="$2"
  local bin_path="$3"
  local bin_escaped config_escaped
  bin_escaped="$(escape_sed "$bin_path")"
  config_escaped="$(escape_sed "$CONFIG_DIR")"

  sed \
    -e "s|__BIN__|$bin_escaped|g" \
    -e "s|__CONFIG_DIR__|$config_escaped|g" \
    "$template" >"$dest"
}

main() {
  local bin_path
  if ! bin_path="$(resolve_binary)"; then
    echo "Unable to find codexclaw binary." >&2
    echo "Set CODEXCLAW_BIN or build the project first." >&2
    exit 1
  fi

  mkdir -p "$CONFIG_DIR"

  case "$OS" in
    darwin)
      local launch_agents_dir dest plist_label
      launch_agents_dir="$HOME/Library/LaunchAgents"
      dest="$launch_agents_dir/$LABEL_MAC"
      plist_label="$LABEL_MAC"
      mkdir -p "$launch_agents_dir"
      render_template "$ROOT_DIR/deploy/com.codexclaw.agent.plist" "$dest" "$bin_path"

      launchctl bootout "gui/$(id -u)/$plist_label" >/dev/null 2>&1 || true
      launchctl bootstrap "gui/$(id -u)" "$dest"
      launchctl enable "gui/$(id -u)/$plist_label" >/dev/null 2>&1 || true
      launchctl kickstart -k "gui/$(id -u)/$plist_label"
      echo "Installed $plist_label at $dest"
      ;;
    linux)
      local systemd_dir dest
      systemd_dir="$HOME/.config/systemd/user"
      dest="$systemd_dir/$LABEL_LINUX"
      mkdir -p "$systemd_dir"
      render_template "$ROOT_DIR/deploy/codexclaw.service" "$dest" "$bin_path"

      systemctl --user daemon-reload
      systemctl --user enable --now "$LABEL_LINUX"
      echo "Installed $LABEL_LINUX at $dest"
      ;;
    *)
      echo "Unsupported operating system: $OS" >&2
      exit 1
      ;;
  esac
}

main "$@"
