---
name: codex-live-turns
description: Handle live Codex app-server turn events for persistent threads. Use when changing how Codex messages are streamed, buffered, scoped to thread/turn IDs, or forwarded to Telegram.
---

# Codex Live Turns

Use this skill when a task touches live Codex turn execution rather than simple request/response behavior.

## When This Skill Matters

- Intermediate messages arrive too late, too early, duplicated, or for the wrong chat.
- A change uses raw app-server notifications instead of high-level SDK helpers.
- A turn hangs, finishes early, or picks up events from another thread.
- You need to reason about `threadId` and `turnId` matching, buffering, or replay.

## Working Rules

- Treat the app-server event stream as process-wide unless proven otherwise.
- Do not trust thread-scoped notifications to identify the active turn before the actual `turn_start` result is known.
- Before the turn ID is known, buffer only what is needed and replay it once the turn is identified.
- Once the turn ID is known, require both `threadId` and `turnId` for item and delta events.
- Be conservative with lifecycle events. A wrong terminal match is worse than a delayed flush.

## Practical Workflow

1. Identify the source of truth for the turn ID.
2. Separate “buffer before turn ID” logic from “handle live event for known turn” logic.
3. Keep delta aggregation state independent from completed-item forwarding.
4. Make terminal behavior explicit:
   - what ends the turn,
   - what only updates state,
   - what is ignored.
5. Add or update regression tests for thread/turn scoping whenever matching logic changes.

## Validation

- Run narrow unit tests for delta aggregation and strict thread/turn matching first.
- Run the ignored live e2e only as a sanity check, not as the sole proof, because live-model behavior can vary by prompt and model.
- If a daemon bug was involved, restart the service and verify the health endpoint after rebuilding.

## Avoid

- Do not broaden event matching to “missing fields means match” for item or delta notifications.
- Do not rely on final `ItemCompleted` events alone if the feature requires intermediate user-visible output.
- Do not duplicate large chunks of SDK request-building logic unless there is a concrete runtime reason and no smaller seam exists.
