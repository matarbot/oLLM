# Answers to REVIEW-FOLLOWUP.md (F1–F8)

Supersedes nothing except where it refines `REVIEW-ANSWERS.md` §1/§5 (the W-table). Principal
input since the questions were asked: **hermes keeps a stable prompt thread until compaction**
(accepted, with a cheap empirical check added to Phase 0), and **the 27B unit is the v1
dev/acceptance target** (it gives the box the headroom the build phase needs; Flash-Next is
stopped for it).

## F1 (blocking) — what makes the *active* conversation durable

**Answer: W1 already covers it, via the destroy site the W-table was missing (`:1744`), and one
new trigger (`finish`) covers `--cache-ram 0`.**

- For a continuing hermes thread with L1 on, turn k+1's admission finds the cached turn-k entry
  **subsumed** by the longer prompt, so `server_prompt_cache::alloc()` erases it at
  `server-task.cpp:1744`. That erase is *the* per-turn publish point: it fires on almost every
  turn, the bytes are already serialised host-side (free), and the erased state is exactly the
  latest completed turn. So `kill -9` after turn k, restart, first request reuses turn k —
  **with no new trigger, on both current units (both have `--cache-ram > 0`)**. This is what
  makes T6 pass in the L1-on configuration.
- The hole is `--cache-ram 0` (no L1 ⇒ no W1). New trigger:
  **W4 `finish`** — at generation end (task completion, not global idle), extract and publish if
  the prefix grew since last publish by ≥ `--cache-disk-publish-stride` (default 2048 tokens) or
  ≥ ×2 in length. That is option (a) with option (b)'s growth gate; (c) is unnecessary. The
  extraction is gated like W2: `queue_tasks.empty()` and no *other* slot decoding, so it never
  delays another slot. It is the only publish source for L1-off runs.
- **W2p `cold`** stays, narrowed to fire only for the *first* publish of a conversation
  (`n_prefill ≥ --cache-disk-min-tokens` and no existing entry for the prefix). That is the only
  case where an on-path extraction is worth it: the whole recompute cost is being paid right now
  and nothing is on disk yet. Still byte-capped.
- **Write amplification is real and is handled by pruning, not by refusing to publish.** W1's
  per-turn entries make a 100k-token thread O(n²) bytes (~3.5 GiB per entry at the tail). With a
  300 GiB budget that is survivable but not free. Prune policy (new, goes in §6.9): under budget
  pressure, prefer deleting an entry whose **same-chain descendant still exists** (it only serves
  shorter replays), keeping per chain roughly the newest plus one per doubling. Measure with
  `waste_bytes = written_bytes / ideal_bytes`; the plan's write-amp target gets a number from the
  30-turn replay benchmark, not a promise.
- The principal's crash-loss knob is `--cache-disk-publish-stride` (L1-off only). Default 2048
  tokens until told otherwise.

Revised W-table (replaces `REVIEW-ANSWERS.md` §1 / `design.md` §5):

| id | trigger | cost | default |
|---|---|---|---|
| **W1** | L1 eviction, all **five** destroy sites (§F2) | free | on |
| **W2** | slot idle, queue empty | device transfer | off |
| **W3** | **cold prefill**, first publish of a conversation only | device transfer, on-path, byte-capped | on |
| **W4** | **generation end**, growth-gated (stride or ×2), no other slot decoding | device transfer | on when `--cache-ram 0`; on otherwise if `finish` listed |
| **W5** | shutdown drain | none | on, 2 s |

## F2 — five destroy sites, ownership before erase

Accepted in full. The sink wraps **all five** of `alloc` subsumption `:1744`, `alloc`
make-room `:1756`, `load` consume `:1864`, `update` trim-size `:1875`, `update`
trim-tokens `:1890`, and the callback runs **before** the erase/move and takes ownership
(`std::move(prompt)`). Concretely at the consume site, the hook sits *before*
`prompt = std::move(it_best->prompt)` (`:1862`), because after the move the tokens are empty.
Unit test: each of the five paths hands off exactly one owned `server_prompt`, and a sink
failure (queue full) does not block the L1 operation — it drops and counts, never throws back.

## F3 — dedup direction, invariant

Accepted, stated verbatim and put in `design.md` §6.6:

> L2 never writes a key it already holds or has in flight. The check is on the **final key** and
> happens **synchronously at the handoff**, before any bytes move — it is never deferred to the
> writer thread.

Implementation: an in-memory `unordered_set` of stored keys (seeded by the startup scan) plus an
in-flight set; updated at publish and at prune. ~32 B per entry. Because the check is on the
final filename (which includes the unit), cross-unit duplicates are correctly *not* deduped. The
restore→L1→evict→W1 cycle therefore writes nothing: W1 sees its key is stored, drops.

## F4 — chain invalidation rule

Accepted, stated as: **any change to the token prefix other than an append at `n` rebuilds the
chain from scratch.** Triggers, enumerated for the implementer:

| event | effect |
|---|---|
| decode / append at `n` | only valid extension — chain extends |
| `prompt_clear()` | chain reset, empty |
| slot handed to a different conversation | record zeroed on reassignment |
| prompt truncation (request over window) | rebuild |
| context shift / `seq_rm` of a prefix region | rebuild, **and the shift count enters the identity**: a shifted conversation lives in a different key space than an unshifted one, so the old and new chains never match each other |
| `seq_add` with a position shift | as shift |

A wrong chain here is a silently-wrong restore that the compat key passes, so: unit test
"shift-then-publish must not match the pre-shift entry; append-then-publish must match", plus a
property test that the chain is a pure function of (compat_key, shift_count, tokens).

## F5 — `--cache-disk-on` values and defaults

Value set: `evict, finish, cold, idle, shutdown` (W1, W4, W3, W2, W5).
Default: `evict, finish, cold, shutdown`. (`idle` stays opt-in, mirroring
`--cache-idle-slots`; `finish` subsumes most of it in practice but `idle` still catches slots
freed without a completion, e.g. aborted requests.)
And the safety rule the question asks for: **`--cache-disk-on evict` with `--cache-ram 0` is
refused as a no-op** — at startup we log `cache_disk: --cache-ram 0 → forcing 'finish' into
--cache-disk-on` and add it. The flag alone must never silently disable the feature.
`design.md` §8 gets the updated table (the stale `prefill,release,idle` list is gone).

## F6 — per-unit directory details

All three interactions settled:

1. The symlink refusal covers **both** `<root>` and `<root>/<unit>` (the leaf is the unit dir).
2. `--cache-disk-prune` and `DELETE /cache/disk` operate on **this server's unit only**. A
   server must never delete another server's entries; there is no `prune-all-units` flag — the
   operator escape hatch is `rm -rf <root>/<unit>/` while that server is stopped (documented).
3. Default unit name: `<model-basename>-<sha8>`, where `sha8 = sha256(compat_key)[0:8]`.
   That isolates quota per (model, *configuration*) — two units serving the same file with
   different `--ctx-size`/`--parallel` get separate dirs, which is correct because their entries
   are incompatible anyway. The dir gets a `.unit` metadata file (human-readable model path +
   the flag-derived identity fields) so the sha8 is decodable by eye.

## F7 — tier attribution

Accepted. Restored counters gain `tier=l1|l2` (tokens, bytes, ms). T6's pass criterion becomes
explicitly tier-specific: after `kill -9` + restart, **`restored_tokens{tier=l2} ≥ 0.9 ×
prompt_tokens`** — and the `--cache-ram 0` run is the clean measurement where any hit is L2 by
construction, so it is the one that proves the tier itself, while the L1-on T6 proves the
integration.

## F8 — stale budget text

Accepted, fixed in this commit: `HANDOFF.md` §3 (the "10 GiB is trivially fine" line and the
entry-count table), `container/README.md` (`--cache-disk 307200`, root `~/.cache/llama-fork/`
with per-unit subdir), `design.md` §3.3/§6.10.

---

## Principal's answers, recorded

- **Stable hermes prefix (Q1):** accepted from experience, but the design still has to *measure*
  it. Phase 0 gains T0.0: log the first ~6k tokens' chunk hashes for two consecutive hermes
  turns (server-side, one debug flag) and assert prefix-stability; also note where compaction
  cuts (compaction starts a new chain — old entries simply stop being used, no correctness
  issue, they prune out).
- **Dev target = 27B unit (Q2):** v1 acceptance (T6 etc.) runs on `llama-server-27b`
  (`--parallel 3`, so `n_ctx_seq = 262144`, per-conversation attention ceiling ≈ 8.5 GiB, MTP
  draft blob in every entry). Flash-Next is stopped for the build phase; its numbers come back
  in a later pass.
- Still open from the earlier list: Q3 (crash-loss knob — defaulting to 2048-token stride), Q4
  (upstream intent), Q6 (backup/snapshot policy for `~/.cache`).

## Doc edits applied with this commit

`design.md` §5 (new W-table incl. W4/W5), §6.6 (dedup invariant), §6.9 (chain-aware prune),
§8 (flags: `--cache-disk-unit`, `--cache-disk-on` values/defaults, publish-stride), §3.3/§6.10
(per-unit dirs + 300 GiB + `n_ctx_seq` nuance), §9 (`draft_present`), §6.3/6.4 (C4/C5);
`implementation-plan.md` (hook placement fixed to `server_context_impl`, five destroy sites,
W-ids, T0.0, phase reorder, `can_add` PR, null-guard fix, budget); `HANDOFF.md` §3;
`container/README.md` (F8); `REVIEW-ANSWERS.md` (DONE/TODO inline marks).
