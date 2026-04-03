#!/usr/bin/env bash
set -euo pipefail

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
UID_NUM="$(id -u)"

case "$OS" in
  darwin)
    launchctl kill HUP "gui/$UID_NUM/com.hooyodex.agent" || \
      launchctl kickstart -k "gui/$UID_NUM/com.hooyodex.agent"
    ;;
  linux)
    systemctl --user reload-or-restart hooyodex.service
    ;;
  *)
    echo "Unsupported operating system: $OS" >&2
    exit 1
    ;;
esac
