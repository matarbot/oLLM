# T0.0 — hermes prompt prefix stability

Date: 2026-08-28. Target: `qwen3.8-27b` unit on 127.0.0.1:1245 (production flags).
Method: byte-logging TCP relay on 127.0.0.1:1244 -> 1245 (raw bytes per direction,
no HTTP parsing), `~/.hermes/config.yaml` `base_url` temporarily pointed at 1244
(backup kept, restored after). Two consecutive turns of one real session, plus
earlier one-shot runs.

## Verdict

**Append-stable within a session: CONFIRMED (2 turns, byte-level).**

Captured session `20260828_183830_b6549a`, two `hermes chat -q` turns:

| | turn 1 | turn 2 |
|---|---|---|
| messages | `[system, user]` | `[system, user, assistant, user]` |
| system prompt | 21644 chars | byte-identical to turn 1 |
| user 1 | 38 chars | byte-identical to turn 1 |
| appended | - | `{"role":"assistant","content":"ok"}` + user 2 (38 chars) |
| body size | 66014 chars | 66125 chars (+111 = exactly the appended turn) |

All top-level fields identical between turns: `model`, `max_tokens` (65536),
`stream` (true), `stream_options` (`{"include_usage": true}`), `tools` (20
functions, byte-identical arrays).

So a continuing hermes conversation is a pure prefix extension at the message
level, and the chain-hash identity model (hash of the full prompt prefix) is
valid for the steady state of a session.

## Volatility found (each one re-roots the chain)

1. **Per-session block in the system prompt tail** (stable within a session,
   differs across sessions - expected, a new session is a new chain anyway):

   ```
   Conversation started: Friday, August 28, 2026 (CEST, UTC+02:00)
   Session ID: 20260828_183830_b6549a
   Model: qwen3.8
   Provider: custom
   Platform: cli
   ```

   No clock/time-of-day or per-turn counter in the system prompt (regex
   checked: no `HH:MM:SS`, no per-turn dates). The only date is the fixed
   "conversation started" line.

2. **Skill loading rewrites the system prompt.** The prompt carries an
   `<available_skills>` list and ends with "Only proceed without loading a
   skill if genuinely none are relevant to the task." When a skill is loaded
   mid-session the system prompt mutates at position 0 -> the whole chain
   re-roots -> full re-prefill at that turn. Not observed in this capture
   (0 tool calls); expected to happen at most a couple of times per real
   session. Cost is bounded: one 22k-token re-prefill per mutation.

3. **Reasoning is not replayed.** Turn 1's response included a reasoning
   block (visible in the TUI), but the assistant message stored in the
   session and resent in turn 2 is `{"role":"assistant","content":"ok"}` -
   no `reasoning_content`, no extra fields. The replayed history is
   deterministic. (Hermes-side choice; if it ever started replaying
   reasoning text, prefixes would grow non-deterministically. Worth a
   re-check if hermes upgrades.)

## Side traffic (irrelevant to the cache design)

Each session start also fires a small independent `POST /v1/chat/completions`:
hermes' **session-title generator** (own 942-char system prompt,
`temperature 0.3`, `response_format` set, user msg = the opening message).
~200 tokens, no prefix relation to the main conversation - below any caching
threshold, separate chain. Ignore.

## Open (monitor, not blocking)

* **Where compaction cuts**: 2 turns is far below any compaction threshold.
  The plan's "compaction cuts somewhere past 128k" remains unmeasured. If
  compaction rewrites history in place (a `_compressed_summary` column exists
  in hermes' `messages` table), everything after the cut re-roots. Ask the
  principal / watch over days; the design already degrades gracefully (miss,
  full re-prefill, new chain grows from the mutation point).
* This test had **0 tool calls**. Real conversations append `assistant
  tool_calls` + `tool` results - expected append-only, but the first
  tool-heavy session through a future capture would confirm.

## Artifacts and side effects

* Raw captures: `/tmp/t00-capture/conn-024-183831.log` (turn 1),
  `conn-030-184001.log` (turn 2), `conn-025/conn-031` (title generator).
  Parsed bodies: `/tmp/t00-chat-both.json`. Proxy: `/tmp/t00-proxy.py`
  (fixed twice: str-vs-bytes log write, then decode; both bugs truncated
  the *relayed* response after the first chunk - a silent half-relay, not a
  clean failure).
* Hermes config: restored to `http://127.0.0.1:1245/v1` (backup
  `/tmp/hermes-config.yaml.bak`).
* Session state: 5 throwaway hermes sessions created in
  `~/.hermes/state.db` (`20260828_18*`, all "Reply with exactly..." probes in
  `/tmp`). No user session was touched - `--continue` on `-z` one-shots
  creates a *new* session (verified in `state.db`), so the user's
  `20260826_153150_89a4a3` (in `/home/rain/source/setup`) was never resumed.
* One `request_dump_20260828_181304_*.json` (74 KB) in
  `~/.hermes/sessions/` from the first broken-proxy attempt (hermes writes a
  dump on API failure); it is a bonus full-body capture.

## Consequence for the design

* Chain-hash identity over the full prompt prefix: **keep as designed.**
* No per-turn system-prompt mutation in the common path -> a 16k-32k prompt
  stays cache-hit from turn 2 to session end. This is exactly the workload
  the 300 GiB L2 targets.
* Skill-load re-roots are the main intra-session invalidator; they are
  infrequent and each costs one re-prefill. Nothing to change in v1.
* T0.0's gate condition (append-stable or escalate) is **satisfied** -
  Phase 1 may proceed on the identity model.
