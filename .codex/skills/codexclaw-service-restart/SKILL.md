---
name: codexclaw-service-restart
description: Restart, recover, and verify the local CodexClaw daemon for this repository. Use when launchd/systemd job state is ambiguous, a restart must be proven by PID change, health checks must be verified, or the local service template may need repair.
---

# CodexClaw Service Restart

Use this skill for this repository's local CodexClaw daemon only.

## What This Skill Covers

- Detect the active CodexClaw service instead of assuming a single label.
- Restart the correct user service and prove it with a PID change.
- Recover from the common macOS failure where `launchctl kickstart` leaves the job in `spawn scheduled` or `last exit code = 1`.
- Repair broken local service templates when the plist or unit is missing required `PATH`, log paths, or the expected config wiring.
- Verify the service with both local health endpoints and service-manager state.

## Load These Resources

- Read [references/runtime-layout.md](references/runtime-layout.md) first. It defines the sanitized path conventions, expected labels, and health endpoints.
- On macOS, also read [references/launchd-layout.md](references/launchd-layout.md). It covers the real failure mode where `kickstart` succeeds but the service never stabilizes.
- Read [references/recovery-playbook.md](references/recovery-playbook.md) before changing plist or unit contents.
- Use [scripts/restart_codexclaw_service.sh](scripts/restart_codexclaw_service.sh) as the primary macOS entrypoint. It can print status, restart the active launchd job, and repair a broken plist from bundled templates.
- Use [scripts/detect-active-service.sh](scripts/detect-active-service.sh) when you only need fast manager/PID detection without repair.
- Use [scripts/restart-launchd-service.sh](scripts/restart-launchd-service.sh) on macOS when you need the low-level `bootout/bootstrap/kickstart` sequence directly.
- Use [scripts/restart-systemd-service.sh](scripts/restart-systemd-service.sh) on Linux when `systemd --user` is the active manager.
- Use the concrete sanitized examples in [assets/plists/dev.codexclaw.agent.plist](assets/plists/dev.codexclaw.agent.plist), [assets/plists/com.codexclaw.agent.plist](assets/plists/com.codexclaw.agent.plist), and [assets/systemd/codexclaw.service](assets/systemd/codexclaw.service) when you need a known-good target shape.
- Use the render templates in [assets/plists/dev.codexclaw.agent.plist.template](assets/plists/dev.codexclaw.agent.plist.template) and [assets/plists/com.codexclaw.agent.plist.template](assets/plists/com.codexclaw.agent.plist.template) when the script must rewrite a local plist.

## Working Rules

- Treat the service manager as the source of truth for the active job label, but confirm with the actual listening process on the health port.
- Do not assume `dev.codexclaw.agent` is active. Check `com.codexclaw.agent` too, then fall back to the port listener and its command line.
- Do not call a restart complete unless the old process exited and a new PID replaced it.
- If `kickstart` succeeds but the service stops listening or lands in `spawn scheduled`, escalate to the recovery flow: inspect logs, inspect the plist, repair the template if needed, then `bootout/bootstrap/kickstart`.
- Keep templates sanitized. Use `$HOME`, `%h`, and `$(id -u)` in instructions and resource files; never hardcode a real username or home directory.

## Practical Workflow

1. Detect the active service:
   - macOS: `scripts/restart_codexclaw_service.sh status`
   - fallback: `scripts/detect-active-service.sh`
2. Capture:
   - service manager (`launchd` or `systemd`),
   - active label or unit,
   - current PID,
   - start time,
   - command line,
   - config path.
3. Restart using the appropriate script:
   - macOS: prefer `scripts/restart_codexclaw_service.sh restart`.
   - Linux: prefer `scripts/restart-systemd-service.sh`.
4. Re-read service state and confirm the PID changed.
5. Verify `healthz` and `readyz`.
6. If health fails:
   - inspect logs,
   - inspect the installed plist or unit,
   - compare against the sanitized templates,
   - repair only the minimal missing fields,
   - re-bootstrap or daemon-reload and restart,
   - re-run health checks.

## Validation

- Use the scripts first; they are the narrowest deterministic checks for this skill.
- After any plist or unit repair, run the relevant restart script and verify both `healthz` and `readyz`.
- On macOS, report the old PID, new PID, active label, and whether the repair path was required.
- On Linux, report the old PID, new PID, active unit, and the `systemctl --user` state summary.

## Avoid

- Do not restart a guessed label without proving that it owns the live port or is the active job.
- Do not stop after a green `launchctl kickstart` if the new PID never appeared.
- Do not overwrite a local plist or unit wholesale if only `PATH`, log paths, or config wiring are missing.
- Do not leave duplicate or conflicting repair paths in the skill without making one of them the documented primary entrypoint.
- Do not embed private paths, usernames, bot tokens, webhook secrets, or machine-specific absolute paths in this skill.
