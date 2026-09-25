# Final verification of `1a47c99b1` (F1-F8 answers + doc edits)

Third pass. All F1-F8 answers are sound. I re-derived the per-turn publish flow against
`tools/server/server-task.cpp` / `server-context.cpp` and verified the doc edits landed
(`git show 1a47c99b1` + grep). Remaining items are one role inversion to correct, two
workload-specific decisions, one RAM-budget fact, and one wording fix. Nothing blocks
Phase 0.

## 1. Verification verdict

* **Landed and correct:** revised W-table (design §5, W1/W2/W3/W4/W5), §6.6 dedup invariant,
  §6.9 chain-aware prune, §8 flags (`--cache-disk-unit`, `--cache-disk-on` values/default,
  `--cache-disk-publish-stride`, force-add of `finish` under `--cache-ram 0`), §3.3/§6.10
  per-unit dirs + 300 GiB + `n_ctx_seq` nuance, §9 `draft_present`, §6.3/6.4 C4/C5;
  plan: five destroy sites with exact lines (`1744/1756/1864/1875/1890` - matches
  `grep -n "states.erase|states.pop_front"`), T0.0, T6 tier-specific criterion, T6b,
  hook placement fixed to `server_context_impl`, `can_add` + null-guard as separate PRs;
  container: `--cache-disk 307200`, root `~/.cache/llama-fork/` with per-unit subdir;
  HANDOFF budget text fixed.
* **F1's mechanism holds.** I traced one full turn with L1 on: `get_free_slots` ->
  `prompt_save` (`alloc` subsumption `:1737-1748` erases P_{k-1}.., inserts P_k) ->
  `prompt_load` (consume `:1864` erases P_k). W4 publishes P_k at generation end; W1 fires at
  both erases. `kill -9` after turn k is therefore covered by W4 (the writer has a 2 s
  graceful drain and, even under a kill, a 0.7 GiB file flushes in well under a second).
  Good.

## 2. One role inversion to fix (plan :187, commit message, ANSWERS-2 §F1)

As written: "With L1 on, W1's subsumption erase (`:1744`) is what makes T6 pass per-turn."
That is inverted. In the L1-on steady state **W4 does the publishing**; W1's erases fire
per turn but are **dedup no-ops** for P_k (W4 already stored the key) and for P_{k-1} (same).
W1's real jobs are:

* **backfill** - when W4's growth gate skips a turn, P_k is not stored; it is then captured
  by W1 at the *consume* site `:1864` on the next admission (and P_{k-1} at subsumption
  `:1744`). W1 is what keeps coverage complete;
* **eviction persistence** - the `:1756/:1875/:1890` sites.

Both still true, but the docs should say "W4 publishes, W1 backfills W4's gate skips and
persists L1 evictions". The cost of the inversion: an implementer reading the plan will
measure and optimise the W1 sink (a `std::vector` move, trivially cheap) instead of the W4
path (the only device transfer), and T6's report will show `publishes{source=finish}` doing
the work while the docs point at `evict`.

## 3. Two workload-specific decisions (need principal or planning-agent sign-off)

**Q-A (gate).** W4 is gated on `queue_tasks.empty()` **and** no other slot decoding. On this
box hermes and the coding agent hit port 1245 concurrently, so at generation end the queue is
often *not* empty: W4 skips, and the conversation is persisted only when L1 later evicts it -
which, with the 8 GiB default and a handful of concurrent threads, may never happen before a
crash. Concurrency then loses exactly the turns we promised. Two facts make the gate relaxable:

1. **This is unified memory.** The KV lives in LPDDR5X; `ggml_backend_tensor_get` is a
   same-RAM copy, not a PCIe transfer. For 16k tokens with `--flash-attn on` (32 contiguous
   calls) the extraction is plausibly **single-digit to low-tens of ms** - T0.2 must measure
   it, but the "35 ms memcpy" figure from the first draft was the wrong *order* in both
   directions for the right reason (it is a copy, but in-RAM and batched). The only genuinely
   expensive case remains `v_trans` (16k calls), which we never run.
2. **The codebase already pays this class of cost:** `prompt_save` inside `get_free_slots`
   does a `FLAGS_NONE` `llama_state_seq_get_data_ext` on the server thread at *every*
   admission today. W4 adds at most one such extraction per finished request.

Proposed: relax W4's gate to **no *other* slot decoding** (drop `queue_tasks.empty()`), keep
the byte cap. If T0.2 comes back ugly, revert - the gate is the safety valve. Either way,
state the relaxed gate's cost in the plan's risk table, because as written T6 (single thread)
passes while the real two-client workload silently loses durability.

**Q-B (chain retention under a comfortable budget).** "Newest plus one per doubling" is
correct as a pressure-adaptive policy, but its comfortable-budget cost is large: a 100k-token
thread publishes at 2048, 4096, 6144, ..., ~100k (~48 entries), whose *lengths* sum to
~2.5M tokens, i.e. **~80 GiB of a 300 GiB budget for one conversation**, of which only the
last entry (3.5 GiB) is ever used by a straight replay. Under pressure the same-chain
preference degrades this to ~latest-only, which is why it is survivable. Two things to
confirm, both cheap:

1. the prune preference applies **at eviction selection** (never retroactively, so a hit can
   never race a prune) - state it as an invariant next to the §6.6 dedup one;
2. who consumes the intermediate entries? A straight replay uses only the newest; only a
   **branching** replay (harness rewinds to an older turn) uses the older ones. If hermes
   never branches in v1, the comfortable-budget default could be "newest only, one per
   doubling under no pressure" or simply "newest only" - ~76 GiB per 100k thread of
   difference. Ask the principal which hermes actually does.

## 4. RAM budget fact (from `free -h`: 121 GiB total, 114 used, 6.6 available)

The 27B unit currently passes `--cache-ram 32768`. With the principal's decision to keep the
L1 default at 8 GiB, **dropping that flag (freeing ~24 GiB) belongs to the acceptance setup**
(setup repo, unit edit + install/reload/restart) - confirm it is on the plan for the moment
L2 ships, because until then 32 GiB of L1 is what the box is running, and it competes with
the model, the KV (ctx 786432), the staging pool (`--cache-disk-staging-mib 4096`) and the
writer queue for the 6.6 GiB that is actually available. Consequence for defaults: consider
staging default `2048` or `min(flag, available/4)` rather than 4096 on a box this full; the
writer queue is already byte-bounded.

## 5. Wording

Plan banner: "Hermes prefix stability is assumed (principal, from experience) and **verified
by T0.0**." T0.0 is a Phase 0 test that has not run; the commit message repeats it. Reword to
"assumed (principal, from experience); T0.0 must confirm it before the T6 acceptance run is
trusted". Same for the commit message if it is amended.
