---
name: codexclaw-service-restart
description: Restart and verify the local CodexClaw daemon for this repository. Use when CodexClaw code changes need to be made live, when launchd status is ambiguous, or when CodexClaw health checks must be verified after a restart on this machine.
---

# CodexClaw Service Restart

Use this skill for this repository's local daemon only.

## Known Local Setup

- Treat `dev.codexclaw.agent` as the active launchd job unless current machine output proves otherwise.
- Treat `com.codexclaw.agent` as a separate local plist that may exist but may not be the running service.
- Treat `http://127.0.0.1:4201/healthz` and `http://127.0.0.1:4201/readyz` as the expected health endpoints for the active local config.
- Treat `~/.config/codexclaw/config.toml` as the active config path unless current process output proves otherwise.

## Workflow

1. Confirm the active job before restarting.
2. Capture the current PID and start time of the active process.
3. Restart the correct launchd job.
4. Verify that the PID changed.
5. Verify `healthz` and `readyz`.
6. Report the old PID, new PID, and health-check result.

## Commands

Run these one at a time.

### 1. Confirm the active launchd job

Prefer:

```bash
launchctl print gui/$(id -u)/dev.codexclaw.agent | rg 'state =|pid =|last exit code =|runs ='
```

Check the alternate plist only if needed:

```bash
launchctl print gui/$(id -u)/com.codexclaw.agent | rg 'state =|pid =|last exit code =|runs ='
```

### 2. Confirm the active process

If `dev.codexclaw.agent` is running, inspect its PID directly:

```bash
ps -p <PID> -o pid,lstart,etime,command
```

Expected command shape:

```text
./target/debug/codexclaw --config ~/.config/codexclaw/config.toml run
```

### 3. Restart the service

Use the active job label, normally:

```bash
launchctl kickstart -k gui/$(id -u)/dev.codexclaw.agent
```

- Expect this to require escalation outside the sandbox.
- Ask for escalation instead of claiming the restart happened if `launchctl` returns `Operation not permitted`.

### 4. Verify the restart actually happened

Re-run:

```bash
launchctl print gui/$(id -u)/dev.codexclaw.agent | rg 'state =|pid =|last exit code =|runs ='
```

Then:

```bash
ps -p <NEW_PID> -o pid,lstart,etime,command
```

- Do not call the restart complete unless the PID changed or the old process clearly exited and a new one replaced it.
- If the PID did not change, say so explicitly.

### 5. Verify health

```bash
curl -fsS http://127.0.0.1:4201/healthz
curl -fsS http://127.0.0.1:4201/readyz
```

Expected response:

```json
{"ok":true}
```

## Avoid

- Do not restart `com.codexclaw.agent` when `dev.codexclaw.agent` is the running job.
- Do not treat successful `kickstart` output alone as proof of restart.
- Do not stop after green health checks if the PID never changed when a restart was requested.
- Do not hide sandbox failures; ask for escalation immediately if restart is blocked.
