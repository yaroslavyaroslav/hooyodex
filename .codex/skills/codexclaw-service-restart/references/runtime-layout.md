# Runtime Layout

Use this file as the first reference for the skill.

## macOS

- Preferred service-manager domain: `gui/$(id -u)`
- Known job labels:
  - `dev.codexclaw.agent`
  - `com.codexclaw.agent`
- Common plist locations:
  - `$HOME/Library/LaunchAgents/dev.codexclaw.agent.plist`
  - `$HOME/Library/LaunchAgents/com.codexclaw.agent.plist`
- Common config path:
  - `$HOME/Library/Application Support/codexclaw/config.toml`
- Common log paths:
  - `$HOME/Library/Logs/codexclaw.out.log`
  - `$HOME/Library/Logs/codexclaw.err.log`

## Linux

- Preferred manager:
  - `systemctl --user`
- Common unit name:
  - `codexclaw.service`
- Common unit path:
  - `$HOME/.config/systemd/user/codexclaw.service`
- Common config path:
  - `$HOME/.config/codexclaw/config.toml`

## Health

- Preferred local endpoints:
  - `http://127.0.0.1:4201/healthz`
  - `http://127.0.0.1:4201/readyz`
- Do not assume the port; confirm it from:
  - the live command line,
  - the config file,
  - or the listening socket.

## Required Environment Details

When repairing a service file, keep these settings unless current machine state proves a different value is required:

- `RUST_LOG=codexclaw=info` or `RUST_LOG=info`
- `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`
- On macOS local voice transcription may also require:
  - `$HOME/.local/bin`

## Why These Fields Matter

- `PATH`:
  - Needed for tools such as `cloudflared`, `ffmpeg`, and `parakeet-mlx`.
- `StandardOutPath` and `StandardErrorPath` on macOS:
  - Needed for post-failure diagnosis when `launchctl` reports only `last exit code = 1`.
- Explicit `--config`:
  - Prevents silent drift to another config file.
- Stable `WorkingDirectory`:
  - Makes relative-path assumptions easier to diagnose.
