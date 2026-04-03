# Hooyodex Launchd Layout

Use this file when the restart path is ambiguous, the active label differs from the old skill assumptions, or the launch agent plist needs repair.

## Default Paths

- Home directory: `$HOME`
- Repository root: current repo root; do not hardcode a username into instructions
- macOS config path: `$HOME/Library/Application Support/hooyodex/config.toml`
- Alternate config path seen in older instructions: `$HOME/.config/hooyodex/config.toml`
- Log files:
  - `$HOME/Library/Logs/hooyodex.out.log`
  - `$HOME/Library/Logs/hooyodex.err.log`
- LaunchAgents:
  - `$HOME/Library/LaunchAgents/dev.hooyodex.agent.plist`
  - `$HOME/Library/LaunchAgents/com.hooyodex.agent.plist`

## Labels Seen In This Repo

- `dev.hooyodex.agent`
  - Usually tied to `target/debug/hooyodex`
  - Healthy plist includes:
    - `PATH`
    - `StandardOutPath`
    - `StandardErrorPath`
- `com.hooyodex.agent`
  - Usually tied to `target/release/hooyodex`
  - Healthy plist includes:
    - `PATH`
    - `StandardOutPath`
    - `StandardErrorPath`
    - `HOOYODEX_CONFIG`

Never assume `dev` is active if `launchctl` and `ps` prove `com` is serving port `4201`.

## Required Launchd Environment

These values matter because the service may fail after a restart even if a manual foreground run works.

- `PATH` should include:
  - `/opt/homebrew/bin`
  - `/usr/local/bin`
  - `/usr/bin`
  - `/bin`
  - `/usr/sbin`
  - `/sbin`
  - Optionally `$HOME/.local/bin` if the service needs user-installed tools such as `parakeet-mlx`
- Logs should be written to the standard `Library/Logs` files above so failures are observable
- `WorkingDirectory` should be a stable absolute path
  - `dev` commonly uses `$HOME`
  - `com` commonly uses the config directory or another stable absolute path

## Common Failure Pattern

Symptom:

- `launchctl kickstart -k ...` succeeds
- old PID disappears
- new PID never stabilizes
- `launchctl print ...` shows `state = spawn scheduled` and `last exit code = 1`
- `curl http://127.0.0.1:4201/healthz` fails

Common cause:

- the plist is missing `PATH` and/or `StandardOutPath` + `StandardErrorPath`

Preferred fix:

1. Capture the broken plist.
2. Compare it with the relevant template in `assets/plists/`.
3. Rewrite the plist with sanitized absolute paths rooted at `$HOME`.
4. `launchctl bootout`
5. `launchctl bootstrap`
6. `launchctl kickstart -k`
7. verify new PID and both health endpoints.
