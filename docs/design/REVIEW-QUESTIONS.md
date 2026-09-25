# Review of the design + questions for the planning agent

Reviewer context: read `HANDOFF.md`, `README.md`, `design.md`, `implementation-plan.md`,
`llamacpp-cache-audit.md`, `omlx-cache-analysis.md`, `container/README.md` at `faaba0ea2` /
`186ca3edb`. Everything below was re-verified against the tree at `5ea87ddad` (line numbers
checked, not trusted) and against the GGUFs / host on box0.

Two new inputs from the principal that are **not** in any doc yet:

* **`--cache-disk` budget on this machine is ~300 GiB**, not 10 GiB (1 TB NVMe, `/home`
  btrfs, 725 GiB free).
* **`--cache-ram` keeps its 8 GiB default**, and we want to test with L1 **active and
  inactive**.
* Acceptance scenario is not hypothetical: the server crashed while this project was being
  planned and a long hermes thread had to be re-processed from scratch. "The harness notices
  nothing" is the definition of done.

## 1. Corrections / additions to the audit (all verified)

| # | finding | where |
|---|---|---|
| C1 | Extraction is **not a memcpy**. `llama_io_write_host` defers every `write_tensor` and runs one `ggml_backend_tensor_get` per (tensor, range) **in its destructor**; restore is symmetric (`ggml_backend_tensor_set` in `llama_io_read_host`'s dtor). With `-ngl -1` the KV is device-resident, so "0.3 GiB memcpy, ~35 ms" (`HANDOFF` §4.7, `design` §4) is really a batch of synchronous DtoH transfers. Must be measured (T0.2), not estimated. | `src/llama-context.cpp:2566-2595`, `:2613-2640` |
| C2 | The cost is **orders of magnitude** flag-dependent: with `v_trans` (i.e. `--flash-attn off`) the value loop issues one transfer per (layer x `n_embd_v_gqa` x range) = 16 x 1024 = ~16k backend calls for one 16k snapshot, vs 32 on the `!v_trans` path. Reinforces keeping `flash_attn` in the compat key; both modes should get a T0.2 number. | `src/llama-kv-cache.cpp:2154-2182` vs `:2183-2216` |
| C3 | The restore path contains `GGML_ASSERT`s **after** a successful `find_slot`. A payload that passes CRC but does not fit the running config can abort the process, not miss. "Validate before `set_data`" therefore needs a real pre-check (`cell_count` / `n_tokens` vs `n_ctx_seq`, blob size vs `get_size_ext`), and test T8 must include *structurally valid but semantically wrong* states, not only bit flips and truncation. | `src/llama-kv-cache.cpp:2270-2276`, `src/llama-memory-recurrent.cpp:793-800` |
| C4 | Restore already does `seq_rm(dest_seq_id, -1, -1)` itself, so `design` §6.4's "always after `mem.seq_rm`" is redundant at the memory level. What genuinely has to be cleared is the slot's bookkeeping (`slot::prompt_clear`, which pairs `mem.seq_rm` with `prompt.clear()`), otherwise the slot believes it has tokens its memory does not have. | `src/llama-kv-cache.cpp:2226`, `src/llama-memory-recurrent.cpp:962`, `tools/server/server-context.cpp:288` |
| C5 | Recurrent restore unconditionally does `set_rs_idx(seq_id, 0)`. Matters if `n_rs_seq != 0` and a slot restores more than once per life. | `src/llama-memory-recurrent.cpp:849-851` |
| C6 | Upstream already asks for exactly this feature at exactly the hook we want: the TODO in `try_clear_idle_slots()` says "move slot to level 2 cache instead of removing". Useful both as an anchor and for upstream framing. | `tools/server/server-context.cpp:1567-1573` |
| C7 | `--cache-ram 0` => `prompt_cache == nullptr` (`:1269-1277`), `--cache-idle-slots` is force-disabled (`:1338-1341`), and the admission path skips the entire cache block via `update_cache = update_cache && prompt_cache` (`:1544`). `:2325` derefs `*prompt_cache` unguarded and survives only because `prompt_save` returns early on an empty slot prompt. **Any L2 hook placed inside `server_prompt_cache` (`design` §2, plan Phase 1 steps 7-8) is dead code when L1 is off** - which is one of the two modes we must support. | `tools/server/server-context.cpp:1269`, `:1338`, `:1544`, `:2325` |
| C8 | Snapshot arithmetic confirmed from the real file (`qwen35`, 65 layers, `full_attention_interval=4` => 16 attn / 49 recurrent, `ssm.state_size=128`, `inner=6144`, `group=16`, `conv_kernel=4`): 3.00 MiB + 120 KiB per recurrent layer => **152.9 MiB**, attention 32768 elem/token => 64 KiB f16 / 34 KiB q8_0. Nuance: `--parallel 3` makes `n_ctx_seq = 786432/3 = 262144` (= the trained length in the GGUF), so one conversation's attention tops out near 8.5 GiB. The "zero entries fit at 786k" line conflates `n_ctx` with `n_ctx_seq`. | `~/models/qwen3.8-27b/*.gguf`, `src/llama-hparams.cpp:183-229` |
| C9 | The MTP draft module is arch `qwen35`, 18 tensors, `block_count=65`, interval 4, and its context gets a **plain attention KV cache** (`mtp_on_hybrid_qwen`), so `state_dft` also grows with `n` and is not a rounding error. T0.2 should size `ctx_tgt` and `ctx_dft` separately; `design` §3.2 reads as if one blob dominates. | `src/llama-model.cpp:2411-2416`, `mtp-Qwen3.8-27B-Q4_0.gguf` |

## 2. The architectural gap: L1 eviction is not a publish trigger

`design` §5's triggers are all slot-driven (prefill / release / idle / shutdown) and §1 lists
"replacing `--cache-ram`" as a non-goal. But L1 destroys entries on its own, in four places,
none of which is a publish point:

* oversized entry skipped entirely - `tools/server/server-task.cpp:1723-1735`
* `pop_front` to make room - `:1750-1758`
* `update()` trim to `limit_size` / `limit_tokens` - `:1870-1885`
* `load()` erases the entry it restores (consume-on-use) - `:1856-1865`

With the 8 GiB default and ~0.7 GiB per 16k-token entry, L1 holds about **11 entries**. A
hermes workload with a few long threads is therefore dominated by *L1 eviction*, not by
restart. If L2 is only fed at slot boundaries, it will never see most of what L1 throws away,
and the disk tier will look like it does not work exactly in the case it was built for.

Suggested change: **L2 is the sink for L1 eviction.** This is also the cheapest write path in
the whole design, and it removes the plan's risk row "memcpy on the server thread stalls
decode": at the moment L1 drops an entry, the state is *already serialised host bytes* - the
handoff is a `std::vector<uint8_t>` move into the writer queue, with no `llama_context` call,
no device transfer, and no extraction dedup problem. Identity (chain hash over the token
prefix) is computed once when L1 snapshots, and travels with the entry.

That gives three distinct write sources, which the design should name explicitly:

1. **L1 eviction** (bytes already in hand, free) - the capacity path;
2. **slot publish** (needs extraction, costs a device transfer) - the only path that works
   when `--cache-ram 0`, and the one that captures state L1 never held;
3. **shutdown drain** - bounded, as designed.

Related: if L2 is a sink, L2 reads must be **non-destructive** (keep the file after a restore)
and entries must be shareable/pinnable, because with a small L1 several slots - or the same
conversation after a crash - will hit one file repeatedly. `design` implies this; it should say
it, because it is the opposite of L1's consume-on-use contract.

## 3. `--cache-ram` active and inactive is a design axis, not just a test

Per C7, the current plan's hook points do not exist when L1 is off. Concretely:

* `get_free_slots` skips the cache block entirely (`:1544`);
* `--cache-idle-slots` is auto-disabled (`:1341`) and its body derefs a null `prompt_cache`
  (`:2325`);
* the token/prefix bookkeeping (`server_prompt.tokens`) lives in L1, so with L1 off there is
  nothing to hash.

So either L2 grows its own token bookkeeping and its own admission hook (more invasive, works
in both modes), or the slot keeps a thin "last prompt + its chain hash" independent of L1
(less invasive). This is the single biggest open decision and it is not in the plan.

Test matrix should gain an explicit axis: for each of `--cache-ram {32768, 8192(default), 0}`
x `--cache-disk {0, N}`: cold correctness, restart reuse, idle-spill behaviour, and the
`--cache-disk 0` "byte-identical to master" guarantee.

## 4. 300 GiB changes these numbers and rules

* §3.3's table becomes: ~428 entries at 16k, ~240 at 32k, ~68 at 128k. Startup header-only
  scan of a few hundred files is still trivial.
* §6.9's "clamp if `--cache-disk` > 50 % of free space": 300 GiB is 41 % of the 725 GiB free
  on `/home`, which is the *same btrfs volume* as the 87 GiB of weights and future downloads.
  Quota should be checked against free space at startup *and* re-checked on `ENOSPC`, and the
  budget should count the in-flight `*.tmp` bytes, because CoW gives no shared extents: peak
  usage is budget + largest entry.
* Write amplification is now a first-order concern, not a phase-2 nicety: filling 300 GiB
  means ~428 publishes of a 16k entry. The plan's "< 1.2x ideal for a 30-turn replay" needs a
  measured number, and phase 2 (segmented attn-tail + full recurrent) is what keeps a 100-turn
  thread from writing ~44 GiB.
* Page cache: hundreds of GiB of buffered writes on a 121 GiB box. Suggest
  `posix_fadvise(POSIX_FADV_DONTNEED)` after write and after read rather than `O_DIRECT`;
  weights are `--load-mode none` (malloc'd), so we are competing with them for RAM and
  bandwidth, not for mmap pages.
* btrfs frees deleted bytes when the transaction commits, so post-crash quota convergence is
  approximate; the quota must be rebuilt from files (already §6.7) and not be assumed exact
  immediately after eviction.

## 5. Durability specifics to settle (stated as the crucial property)

* §6.8 persists `last_access` by rewriting the header **in place** at a fixed offset inside a
  hundreds-of-MiB file. A torn write there invalidates an otherwise perfect payload. Pick one:
  (a) LRU from file mtime only, (b) a 256 B sidecar written atomically (tmp + rename + dir
  fsync), (c) dual header copies with a sequence number. As written, §6.8 is the only place in
  the design where a crash can corrupt an entry.
* §6.2's "delete `*.tmp` older than one boot" can be simpler and safer: the sweep runs while
  we hold the exclusive `flock`, so no other writer exists - delete **all** `*.tmp`
  unconditionally, no clock or boot-time logic.
* Define the durability model in one sentence and stick to it: process-kill (`SIGKILL`, OOM,
  `podman kill`) keeps everything the tmp+`fdatasync`+rename+dir-`fsync` pattern gives; true
  power loss additionally means nothing queued survives. Which one are we promising? The chaos
  suite should say so per test, since a rootless box cannot test real power loss (no
  `/proc/sysrq-trigger` write, no `dm-delay` without root) - so either we accept "kill -9
  without sync" as a proxy or we say power loss is out of scope.

## 6. Questions for the planning agent

1. **L2's write sources.** Do you accept "L1 eviction hands its already-serialised bytes to
   L2" as a first-class publish path (§2 above)? If not, what makes the 8 GiB-L1 case work,
   given L1 holds ~11 entries at 16k tokens?
2. **Where do identity and tokens live when `--cache-ram 0`?** L2 owns `server_prompt`-like
   bookkeeping + its own admission hook, or the slot keeps a thin (prompt, chain-hash) pair
   usable by both tiers? This determines whether the L2 hooks go in `server_prompt_cache` (dead
   when L1 is null, see C7) or in `get_free_slots` / `launch_slot_with_task`.
3. **Do L2 reads stay non-destructive and shareable** (pin/refcount, one file restorable by
   many slots and by N restarts)? L1 is consume-on-use; L2 must not inherit that by accident.
4. **Publish granularity vs cross-session sharing.** v1 publishes at whole-slot prefix
   lengths, so two hermes sessions sharing a 6k system prompt only share it if an entry
   happens to exist at exactly that length. Do we align publish points to `B`-token boundaries
   (or to message spans, currently phase 3) in v1, or accept "restart reuse only" for v1 and
   state it loudly?
5. **Per-publish stall policy.** Given C1 (device transfer, not memcpy): is a stall after
   prefill acceptable, or should extraction happen only when the slot goes idle / at shutdown /
   on L1 eviction (which never stalls)? If a stall is needed, cap it by bytes, not tokens.
6. **Directory layout at 300 GiB.** One directory per unit (`~/.cache/llama-server/<unit>/`)
   with its own budget, or the shared directory where `foreign_bytes` are kept forever
   (`design` §6.10)? With two units and a 300 GiB budget, a shared directory lets one model's
   entries squat on the other's quota. Recommendation: one dir per unit, compat key as a
   second line of defence.
7. **Durability model** per §5: kill-only or power-loss, and which of (a)/(b)/(c) replaces the
   in-place header rewrite in §6.8.
8. **Restore pre-validation contract** (C3): what exactly do we check before calling
   `llama_state_seq_set_data_ext`, and can we get `cell_count` from our header (written at
   extraction time from `get_size_ext` plus a small metadata read) so we never hand the
   asserts a state that cannot fit?
9. **MTP**: is `state_dft` mandatory in an entry (`reject_no_draft`), or do we want a
   target-only restore that runs the request without spec decode? Per C9 the draft blob is not
   small, so this affects both size budget and hit rate.
10. **Defaults.** `--cache-disk` default stays `10240` upstream-style with the unit passing
    `307200`, or does the fork default to something bigger? And do we keep
    `--cache-disk-dir` default `~/.cache/llama-server/` given the toolbox bind-mounts
    `/home/rain` (so the path is host NVMe from inside the container, which is what we want) -
    the container recipe currently uses `~/.cache/llama-server-fork/`.
11. **Metrics that prove the thing.** Beyond hit/miss: is `saved_prompt_tokens` measured in
    tokens *not* prefilled (i.e. `n_restored_tokens`), and do we convert it with the T0.3 pp
    rate? The crash that lost this thread is the acceptance test: first request after restart
    should show `prompt eval time` proportional to new tokens only.

## 7. Small things worth stealing from the audit of the audit

* The oMLX doc's "hot tier disabled by default (`hot_cache_max_size = "0"`)" is direct
  evidence that the tiered design works with the small tier nearly empty - worth citing when
  we argue that L1 can shrink once L2 is real.
* `--cache-ram`'s own `limit_tokens` dynamic (`update()`, `:1870-1885`) means a large
  `size_per_token` shrinks the *token* budget too; a restored 128k entry can be evicted for
  token-count reasons even when bytes are available. L2's policy must not inherit that
  coupling blindly.
* When we touch `common/arg.cpp` near `-cram` (`:1705`), note that `--cache-idle-slots` help
  text says "requires cache-ram" - that sentence becomes wrong once L2 can stand alone.
