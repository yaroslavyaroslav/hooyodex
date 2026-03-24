# CodexClaw

CodexClaw is a small Rust service that connects Telegram webhooks to persistent Codex app-server threads.

## Working Style

- Keep the implementation pragmatic. Prefer the smallest reliable change over abstraction-heavy refactors.
- Preserve the service shape: Telegram gate in, Codex turn execution, Telegram output out.
- Treat end-to-end behavior as the source of truth. A clean unit test suite is not enough if the live bot or daemon behavior disagrees.
- Prefer fixing real runtime bugs over “improving architecture” unless the architecture is the bug.

## Code Changes

- Keep config, transport, and Codex session logic separate.
- Prefer using existing repo patterns before introducing new traits, helpers, or modules.
- Avoid speculative features. Add only what is needed for the current Telegram or Codex workflow.
- Be careful with duplicated request-building logic around Codex SDK calls. If custom event handling forces a lower-level path, keep the duplicated surface as small as possible.

## Telegram

- Whitelist and normalization behavior must stay conservative: ignore unknown senders and unsupported updates quietly.
- Treat Telegram delivery as an integration surface, not just a formatting layer.
- When changing outbound behavior, verify what Telegram actually receives, not only what the code intended to send.
- Do not add bridge-local sandboxing or path restrictions on top of Codex capabilities; if access control matters, rely on Codex's own sandboxing and capability model.
- For live Codex output, Telegram should receive completed user-visible assistant messages, including legitimate intermediate commentary and the final answer, but not raw delta fragments.
- Do not forward plan items to Telegram as chat messages.
- If intermediate Telegram delivery timing changes, prefer lifecycle-based flush points (next item/tool/server event or turn completion) over assumptions that token deltas are globally serial.

## Codex Runtime

- Persistent chat threads matter more than short-term convenience. Do not casually break thread reuse or state persistence.
- When handling streamed or live notifications, scope events carefully to the correct thread and turn.
- Treat `item.id` as the stable identity for a live item only within the active `(threadId, turnId)` scope.
- Do not assume that unrelated live events cannot interleave while an agent message is being assembled unless the protocol or local code proves it.
- If a change depends on model behavior, confirm it with a realistic manual or ignored e2e run and call out that dependency explicitly.

## Validation

- For small changes, run the narrowest test or build command that exercises the change first.
- Before considering work done, run `cargo test`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo fmt --all` unless the user explicitly narrows scope.
- If the daemon is affected, rebuild, restart the user service, and verify the health endpoint.

## Ops

- Assume user-level service management on macOS `launchd` and Linux `systemd --user`.
- Prefer reversible operational changes and keep generated service behavior understandable from the repo.
- Do not assume webhook, tunnel, or model issues are interchangeable; isolate the failing layer before changing multiple layers at once.
