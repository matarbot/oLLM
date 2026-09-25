# Follow-up review of `e043eb54c` (answers + partial design revision)

Second pass. I verified the answers' factual claims against the tree at `5ea87ddad`, verified
which of the 11 "still to apply" edits actually landed, and found one hole in the revised
publish policy. Nothing here re-litigates §1-§5 of `REVIEW-ANSWERS.md`.

## 1. Landed / not landed (checked with `git show --numstat`, then by grep)

Applied in `e043eb54c` (design.md +57/-24, HANDOFF +14/-4, README +15/-2):

* design §2 hooks moved to `server_context_impl`, `cache_disk_mib != 0` guard, slot-owned
  `server_prefix`, non-destructive shareable reads - **good**;
* design §4 extraction = batched synchronous device transfers, with the `v_trans` call-count
  asymmetry, plus the new "cheap path is L1 eviction" invariant - **good**;
* design §5 W1/W2/W2p/W3 table - **good**;
* design §6.2 unconditional `.tmp` sweep, §6.8 header-written-once + mtime/`utimensat` LRU -
  **good**;
* README v1 scope sentence (no cross-session sharing, truncation argument), read order,
  300 GiB budget, doc-index rows for the two REVIEW files - **good**;
* HANDOFF read order + supersession note + invariant 7 rewritten - **good**.

Not applied (grep for the deciding strings returns 0 hits in every doc):

| item from the answers' own list | state |
|---|---|
| 4 - `design` §3.3 `n_ctx_seq = n_ctx / n_parallel`, ~8.5 GiB per-conversation cap, 300 GiB entry counts | absent (`8.5 GiB`: 0) |
| 5 - `design` §6.10 + `--cache-disk-unit` per-unit directories | absent (`cache-disk-unit`: 0 in all four docs; §6.10 still describes one shared directory) |
| 6 - tmp bytes in quota, `statvfs`, `posix_fadvise`, approximate-after-eviction | absent (`posix_fadvise`/`statvfs`: 0) |
| 7 - C9 draft sizing, `draft_present` flag, strict-miss policy in §3.2/§9 | absent (`draft_present`: 0) |
| 8 - C4 / C5 in §6.3/§6.4 | absent (`set_rs_idx`: 0) |
| 9 - **`implementation-plan.md`: phase reorder, new tests, `can_add` PR, null-guard fix** | **file untouched since before the review** |
| 11 - `--cache-disk-on` default list | design §8 still says `prefill,release,idle`, which no longer exists as a trigger set |

The plan file is the one an implementer follows commit by commit, and it still says
`server_prompt_cache` gains a `server_disk_cache * l2` with call sites in `alloc/load/update`
(`implementation-plan.md:65`), step 7 restores "after L1 `load()` finds nothing" (`:95`), and the
review split still puts L2 *in* `server_prompt_cache` (`:182`). That is exactly the placement
`REVIEW-ANSWERS.md` §2 rejected. Until the plan is regenerated, the authoritative statement for
any implementer should be "REVIEW-ANSWERS.md supersedes implementation-plan.md", not just
"supersedes HANDOFF".

Also, `REVIEW-ANSWERS.md` contradicts itself: the *Doc changes* section says "I have written the
answers, not yet re-edited the other four documents", while the same commit rewrites design §2/§4/§5/§6.2/§6.8
and the README. Please mark the 11 items DONE/TODO inline so nobody redoes or misses them.

## 2. Two claims I checked and confirmed (so you can drop them from your risk list)

* **No capacity query exists.** `grep -rn "can_add" include/llama.h src/llama-memory.h
  src/llama-kv-cache.h` -> nothing; `llama_memory_i` (`src/llama-memory.h:83-126`) exposes only
  `init_batch` / `seq_*` / `memory_breakdown`. Your §8 plan (headroom flag now, `can_add` helper
  as its own PR later) is the only option, not a shortcut.
* **`ctx_dft` is one shared draft context** (`server-context.cpp:1086`, assigned to every slot at
  `:1210`), so per-request spec disable really is surgery. Worth noting why §7's
  `draft.present` field is load-bearing: `ctx_dft` is also nulled at `:1197-1200` when spec init
  throws or `spec` is null, so *two runs with identical flags* can differ in whether a draft
  exists. `draft_present` in the header is therefore a correctness field, not metadata - keep it
  in the compat key exactly as §7 already has it.

## 3. New finding: five destroy sites in `server_prompt_cache`, not four

The W1 sink list in `REVIEW-ANSWERS.md` §1 / design §5 is `1723-1735`, `1750-1758`,
`1870-1885`, `1856-1865`. `grep -n "states.erase\|states.pop_front" tools/server/server-task.cpp`
gives five sites:

| line | what | in your list? |
|---|---|---|
| `:1744` | `alloc()`: erase every cached prompt **fully contained in the incoming prompt** | **no** |
| `:1756` | `alloc()`: `pop_front` to make room | yes |
| `:1864` | `load()`: consume the entry it restored | yes |
| `:1875` | `update()`: trim to `limit_size` | yes |
| `:1890` | `update()`: trim to the dynamic `limit_tokens_cur` | collapsed with the above |

`:1744` is the important one: it is the *normal* path for a continuing conversation (the new,
longer prompt subsumes the cached shorter one), so it fires on almost every turn of a hermes
thread. If the sink does not wrap it, we throw away exactly the prefix we most want on disk, and
we do it before the new prompt's own state exists - an unrecoverable miss, not a deferred one.
Note also that `:1744`'s entries are still *valid* (their KV may still live in the slot/cache),
unlike the oversized-skip case.

## 4. The hole in the revised publish policy (F1 below)

With `W2p` off and `W2` gated on "queue empty **and** no slot processing", the entry for the
**currently active conversation** is the least likely one to reach disk:

* W1 fires when L1 evicts - but the hot conversation is the least likely to be evicted (LRU);
* W2 fires when the box is globally idle - a busy harness may never let that happen;
* W3 fires on a graceful stop only.

So the acceptance scenario (T6: long thread, `kill -9` mid-session, restart, first request reuses)
is covered only if L1 is small enough that the conversation *was* evicted, or if the server
happened to be idle after the last turn. Note the original §5 table had a `release` trigger
(in `get_free_slots` before a slot is reused, `:1543-1562`) which is a *different* moment from
global idle, and it disappeared in the W-table.

## 5. Follow-up questions

**F1 (blocking, policy).** What guarantees the *active* conversation is durable before an
unguarded crash, given W1 needs L1 eviction, W2 needs global idle, and W2p is off? Options as I
see them: (a) re-add a per-request-completion publish (the old `release`, or "publish at
generation end when `queue_tasks.empty()`", which is not the same as global idle and is cheaper
than post-prefill because the state is stable); (b) make W2p on but only for prefixes whose
length grew past a threshold since the last publish; (c) periodic per-slot publish behind a byte
budget. Which one, and which test proves it? T6 as written (`kill -9` after each turn) is the
right test - I am asking which publish source is supposed to make it pass.

**F2 (correctness, small).** Confirm the W1 sink wraps all **five** destroys in §3, especially
`:1744`, and that the callback runs *before* the erase and takes ownership (the erase at `:1864`
follows `prompt = std::move(it_best->prompt)` at `:1862`, so a sink hooked after the move sees
empty tokens).

**F3 (dedup direction).** Restore-from-L2 -> copy sits in L1 -> L1 evicts it -> W1 wants to write
the identical file again. Confirm the "already stored / in flight" check happens *before* the
bytes are handed over and that the check is on the final key, so a restore + evict cycle writes
nothing. If it is cheap, state the invariant: "L2 never writes a key it already holds, and W1's
dedup check cannot be deferred to the writer thread" - because a deferred check means a 0.7 GiB
memcpy and a queue slot spent per turn.

**F4 (chain invalidation).** `server_prefix`'s chain is "extended, never recomputed". What
invalidates it? Concretely: context shift / `seq_rm`, `seq_add` with a position shift, prompt
truncation when a request exceeds the window, `prompt_clear`, and a slot handed to a different
conversation. I would like the rule stated as "any change to the token prefix other than an
append at `n` rebuilds the chain from scratch", plus a unit test for shift-then-publish (a wrong
chain here is a silently-wrong restore that the compat key will happily pass).

**F5 (`--cache-disk-on` and defaults).** design §8 still lists `prefill,release,idle`. Once
§5's W-ids are authoritative, what is the flag's value set (`evict,idle,shutdown` plus whatever
F1 decides), and what is the default? Also: does `--cache-disk-on evict` alone have to be safe
when `--cache-ram 0` (no L1, hence no W1)?

**F6 (per-unit dirs, details not yet in any doc).** `--cache-disk-unit` interacts with three
things that are written for a single directory: (i) §8's "refuse to follow a symlink at the leaf"
- the leaf is now `<root>/<unit>`, so the check must cover both components; (ii)
`--cache-disk-prune` and `DELETE /cache/disk` - one unit or all units under the root; (iii) the
default unit name derived from the model basename - `basename + quant` collides when two units
serve the same file with different `--ctx-size`/`--parallel`, which the compat key separates but
the *quota* would then share. Recommend the unit default include the flag-derived identity
(e.g. `sha256(compat_key)[0:8]`) with the human-readable basename as a comment in the directory,
or keep the basename and require `--cache-disk-unit` for anything but a single unit. Which?

**F7 (attribution in the acceptance metric).** With L1 active, a hit can come from either tier.
`n_restored_tokens` alone cannot tell us the disk tier works. Add `tier=l1|l2` to the restored
counters, and make T6's pass criterion require the L2-specific count after a restart (plus the
`--cache-ram 0` run, which is the clean measurement). Agreed?

**F8 (stale budget text).** HANDOFF §3 still says "Our test cache budget (10 GiB) is trivially
fine" and `container/README.md`'s run command passes `--cache-disk 10240` with
`--cache-disk-dir ~/.cache/llama-server-fork/`, while §10 of the answers decides on a 300 GiB
budget and a `~/.cache/llama-fork/` root with per-unit subdirs. Fix in the same pass as items
4-6 so the first implementer does not copy the 10 GiB command line?
