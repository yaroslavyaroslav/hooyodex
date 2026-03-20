# CodexClaw

User-level Telegram gateway for persistent Codex app-server threads.

## What it does

- Runs one long-lived hidden orchestrator thread on Codex startup.
- Keeps one persistent Codex thread per Telegram chat.
- Accepts Telegram text, images, and voice messages through a webhook.
- Downloads attachments into a local cache directory and forwards them to Codex:
  - images as true local-image inputs,
  - voice messages as local file paths referenced in the prompt.
- Forwards each completed Codex agent message back into Telegram.
- Installs as a user-level background service on macOS (`launchd`) or Linux (`systemd --user`).

## Config

Copy [config.example.toml](config.example.toml) to your user config path:

- macOS default: `~/Library/Application Support/codexclaw/config.toml`
- Linux default: `~/.config/codexclaw/config.toml`

## Commands

```bash
cargo run -- run
cargo run -- set-webhook
cargo run -- delete-webhook
cargo run -- service print
cargo run -- service install
cargo run -- service restart
cargo run -- service reload
cargo run -- service status
```

## Service notes

- Linux install target: `~/.config/systemd/user/codexclaw.service`
- macOS install target: `~/Library/LaunchAgents/dev.codexclaw.agent.plist`
- Reload sends `SIGHUP`; the process reloads config from disk for future requests.
