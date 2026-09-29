# Checkpoint persistence across save/restore (llama.cpp ckpt-persist)

Status: implemented on fork branch `ckpt-persist` (base = upstream master, tip at time
of writing 6a2743f02), verified on box0 silicon 2026-09-29. Upstream issue:
ggml-org/llama.cpp#25913.

## The problem

Hybrid / recurrent / sliding-window models (including our production Qwen3.8 build)
could not reuse a restored prompt prefix after a process restart. A server restored
from disk still ran a full prefill on the first request to the restored slot:
22,018 tokens, ~63 s, every time, in a fresh process where the first request is
by definition the first request.

## Root cause (one paragraph)

Prompt-prefix reuse on these memory backends is gated by the slot's checkpoint
ledger (`slot->prompt.checkpoints`). A checkpoint is a saved snapshot of the
recurrent state that lets the server roll back its running summary to a past token
position honestly; without it, partial reuse of a non-rewindable memory would
silently corrupt decoding, so the guard refuses reuse with no valid checkpoint.
The ledger lives only in RAM. `handle_slots_save` serialized the KV/recurrent
state to disk, and `handle_slots_restore` loaded it back — but there was nowhere
to rebuild the ledger from, so the guard reset and forced full re-processing.
The restore "succeeded" and bought nothing.

## The fix (three commits)

1. **Tests** (`tools/server/tests/unit/test_slot_save_checkpoint_reuse.py`,
   plus small plumbing in `tests/utils.py` for `--ctx-checkpoints` /
   `--checkpoint-min-step`): five tests on `tinygemma3` (already in the suite,
   sliding-window, so the same guard as our model). Three assert the fix behavior
   (in-process restore reuse; across-restart restore reuse; sidecar file contract)
   and were red before the fix. Two pin invariants (restore without sidecar
   degrades gracefully; checkpoints-off shows zero reuse) and must stay green.
2. **Save side** (`tools/server/server-context.cpp`): alongside the slot blob,
   write `<blob>.ckpt` — magic `LCKP`, version, then per checkpoint the position
   markers and state blobs. Sidecar write failure is a warning, not an error:
   a missing sidecar means old behavior, which is safe.
3. **Restore side**: read the sidecar, validate magic/version/stream, replay each
   checkpoint through the existing in-memory `load_state(...PARTIAL_ONLY)` path.
   The slot wakes with an earned ledger; the guard trusts it; reuse happens.
   Bad or missing sidecar leaves the ledger empty — degrade, never fail.

Deliberately NOT done: forging `pos_min = 0` after restore. `pos_min` is a
rollback-validity promise; on recurrent state a forged promise corrupts decoding
silently. The checkpoints must be the real saved ones.

## Verified numbers (box0, gfx1151, E12h 2026-09-29)

- seed: raw 22,018-token trunk, native `/completions`, slot 0; save blob
  924 MB + sidecar 313 MB (sidecar size is bounded by `--ctx-checkpoints`).
- fresh process: restore 118 ms.
- first request after restore (`id_slot` pinned in body): `prompt_n=17`,
  0.32 s — the entire prefix served from the restored checkpoint.
- control on a virgin slot, same request: 22,035 tokens, 63 s.
- content byte-identical at temperature 0. **63 s became 0.32 s.**

## Harness pitfalls that masked this (relevant to ALL oLLM slot testing)

Both produce the same "cached=0" signature as the real bug, so they can fake a
fix failure or a fix pass:

1. `/v1/chat/completions` (the OpenAI-compatible route) silently ignores
   `id_slot` passed as a query parameter. The request goes to the dispatcher and
   may land on any slot. Pin the slot via the native `/completions` endpoint with
   `id_slot` in the JSON body.
2. Request options that alter the chat template (`reasoning_effort`, thinking
   flags) fork the rendered prompt text near the head. We measured a 9-token
   longest-common-prefix between a seed saved without `reasoning_effort` and a
   follow-up requested with it. Saved prefix and requested prefix must be built
   with identical template options or nothing can reuse, checkpoint or not.

## What this means for oLLM

- **Cold start story changes.** A restart-provisioned box with a restored forest
  answers the first request in ~0.3 s instead of ~63 s per restored session. The
  forest + factory-seed plan (E12/E12h) is now measured end-to-end on silicon.
- **Blob lifecycle: sidecars are companion files.** Any oLLM proxy operation that
  moves, versions, copies, or deletes a `session__<backend_sig>.bin` blob must do
  the same for `session__<backend_sig>.bin.ckpt`. Atomic publish of a blob
  without its sidecar yields a restorable-but-slow blob (degrades to today's
  behavior); deleting the blob without its sidecar leaks 313 MB per 22k-token
  session per our current measurements.
- **Contention budget.** Sidecar adds ~34% write volume at 22k scale (grows with
  recurrent size and checkpoint count, not with KV size). If disk writes get tight,
  tune `--ctx-checkpoints` / `--checkpoint-min-step` rather than dropping the
  sidecar.
- **Prompt canonicalization is now load-bearing.** Prefix reuse across sessions
  only works if sessions share byte-identical template rendering of their shared
  prefix. Keep option sets uniform across seed/replay paths, or key cache blobs
  on the effective option set, not just model/signature.

## Where to look

- Fork branch: `matarbot/llama.cpp` `ckpt-persist` (three commits, tests first).
- Driver + raw outputs: box0 `~/ollm-cache/e12/e12h.py`, `out_h/`, `raw_h/`,
  `runlog.md`.
- Upstream context: issue #25913; guard added for rewindability in the
  checkpoints work (`--ctx-checkpoints`).
