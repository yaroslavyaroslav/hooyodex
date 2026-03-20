---
name: telegram-bridge-testing
description: Test CodexClaw's Telegram integration through mocked Telegram Bot API calls and targeted runtime checks. Use when changing webhook handling, attachment download, Telegram output delivery, or message formatting.
---

# Telegram Bridge Testing

Use this skill when a change touches Telegram ingress, egress, or the glue between Telegram and Codex.

## What To Test

- Webhook input normalization and whitelist behavior.
- Attachment download behavior through the Telegram Bot API.
- Outbound `sendMessage` and `sendPhoto` behavior.
- Ordering and chunking of user-visible outputs.
- Failure paths that should notify the user versus those that should stay silent.

## Preferred Test Layers

### 1. Transport-level tests

Use a mock Telegram Bot API server and point `telegram.api_base_url` at it.

This is the default choice for:
- verifying what requests Telegram would receive,
- checking parse mode and payload shape,
- testing file download and upload flows.

### 2. Bridge-level tests

Inject a fake turn runner and script the exact outputs you want to forward.

This is the default choice for:
- validating Telegram output logic without paying for model calls,
- reproducing formatting or ordering issues,
- proving that the bug is above or below the Codex boundary.

### 3. Ignored live e2e

Use the real `SessionManager` only when the problem might be in the Codex event stream itself.

This is useful for:
- verifying that intermediate messages actually arrive from the real model path,
- checking whether a fix survives real SDK behavior,
- narrowing model-dependent behavior.

Do not use ignored live e2e as the only proof of correctness.

## Practical Workflow

1. Reproduce the bug with the smallest possible transport-level or bridge-level test.
2. If that stays green while production is broken, add or run the ignored live e2e.
3. When investigating delivery, inspect what the mock Telegram API received rather than only internal logs.
4. If model behavior is unstable, choose sentinel strings that are unlikely to be split or rephrased.

## Validation

- Start with the narrowest `cargo test <name> -- --nocapture`.
- Finish with full `cargo test` and `cargo clippy --all-targets --all-features -- -D warnings`.
- If the webhook daemon changed, rebuild, restart the service, and hit `/healthz`.

## Avoid

- Do not assume a green fake-runner test proves that Codex emitted the right events.
- Do not silently mix transport bugs, formatting bugs, and model-stream bugs into one test.
- Do not build tests around fragile markers like strings that can be split by tokenization if a no-space marker would work.
