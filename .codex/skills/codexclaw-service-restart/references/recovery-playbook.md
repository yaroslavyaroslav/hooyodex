# Recovery Playbook

Use this file when a plain restart is not enough.

## 1. Detect the Active Service

Run:

```bash
.codex/skills/codexclaw-service-restart/scripts/detect-active-service.sh
```

Capture:

- manager,
- label or unit,
- pid,
- command,
- config path,
- plist or unit path if known.

## 2. Decide Between `kickstart` and Full Re-bootstrap

Use plain restart only when all are true:

- the active job is present in the manager,
- it currently has a live PID,
- health is currently green or recently green,
- the installed service file already has sane `PATH` and log wiring.

Use full re-bootstrap when any are true:

- `launchctl print` shows `spawn scheduled`,
- `last exit code = 1` after restart,
- there is no live PID,
- the health port is down,
- the plist lacks `PATH`,
- the plist lacks `StandardOutPath` or `StandardErrorPath`,
- the config path in the job does not match the expected local config.

## 3. macOS Repair Checklist

Compare the installed plist with:

- [../assets/plists/dev.codexclaw.agent.plist](../assets/plists/dev.codexclaw.agent.plist)
- [../assets/plists/com.codexclaw.agent.plist](../assets/plists/com.codexclaw.agent.plist)

Minimal fields that commonly need repair:

- `ProgramArguments`
- `EnvironmentVariables.PATH`
- `EnvironmentVariables.RUST_LOG`
- `StandardOutPath`
- `StandardErrorPath`
- `WorkingDirectory`

Then re-bootstrap:

```bash
.codex/skills/codexclaw-service-restart/scripts/restart-launchd-service.sh \
  <label> \
  <plist-path>
```

## 4. Linux Repair Checklist

Compare the installed unit with:

- [../assets/systemd/codexclaw.service](../assets/systemd/codexclaw.service)

Minimal fields that commonly need repair:

- `ExecStart`
- `ExecReload`
- `Environment=PATH=...`
- `Restart=on-failure`
- `WorkingDirectory`

Then reload and restart:

```bash
.codex/skills/codexclaw-service-restart/scripts/restart-systemd-service.sh \
  codexclaw.service
```

## 5. Verification Standard

Do not declare success until all are true:

- the service manager reports the job or unit as running,
- the old PID is gone,
- a new PID exists,
- `healthz` returns `{"ok":true}`,
- `readyz` returns `{"ok":true}`.

## 6. Reporting Format

Report:

- active manager,
- active label or unit,
- old PID,
- new PID,
- whether a template repair was required,
- health result.
