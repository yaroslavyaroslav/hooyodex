#!/usr/bin/env bash
set -euo pipefail

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
UID_NUM="$(id -u)"

case "$OS" in
  darwin)
    launchctl kickstart -k "gui/$UID_NUM/com.codexclaw.agent"
    ;;
  linux)
    systemctl --user restart codexclaw.service
    ;;
  *)
    echo "Unsupported operating system: $OS" >&2
    exit 1
    ;;
esac
