# CodexClaw

User-level Telegram gateway for persistent Codex app-server threads.

## What it does

- Runs one long-lived hidden orchestrator thread on Codex startup.
- Keeps one persistent Codex thread per Telegram chat.
- Accepts Telegram text, images, and voice messages through a webhook.
- Downloads attachments into a local cache directory and forwards them to Codex:
  - images as true local-image inputs,
  - voice messages as locally transcribed text using `parakeet-mlx` with `mlx-community/parakeet-tdt-0.6b-v3`,
  - the original voice file path is still included in the prompt for debugging and fallback context.
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

## Voice Recognition On macOS

CodexClaw uses the local `parakeet-mlx` CLI for Telegram voice messages. The default model is `mlx-community/parakeet-tdt-0.6b-v3`.

Install the prerequisites:

```bash
brew install ffmpeg
uv tool install parakeet-mlx -U
```

Verify the binary is visible:

```bash
~/.local/bin/parakeet-mlx --help
```

If your `launchd` environment does not include `~/.local/bin` in `PATH`, either:

- keep the default fallback behavior in CodexClaw, which already checks `~/.local/bin/parakeet-mlx`, or
- set `voice.transcriber_command` in your config to an absolute path such as `"/Users/<you>/.local/bin/parakeet-mlx"`.

## Service notes

- Linux install target: `~/.config/systemd/user/codexclaw.service`
- macOS install target: `~/Library/LaunchAgents/dev.codexclaw.agent.plist`
- Reload sends `SIGHUP`; the process reloads config from disk for future requests.
