# llama.cpp cache mechanics (audit at `5ea87ddad`)

Audit target: this fork at `master` = upstream `5ea87ddad` (2026-08-25). All line numbers
are from that commit. Goal: establish exactly what is cached today, what is lost and when,
and which APIs a disk tier can be built on — plus the constraints imposed by our models
(`qwen35` = Qwen3.8-27B, `qwen4exp` = Qwen3.8-Flash-Next), which are **hybrid**
(Gated-DeltaNet recurrent layers + sparse full-attention layers).

---

## 1. Two independent caches

| layer | what | lifetime | where |
|---|---|---|---|
| **context memory** | KV cells / recurrent state, per `seq_id` | process; slots are recycled | `llama_memory_i` (`src/llama-memory.h:74`) |
| **prompt cache** | serialised *seq states* + token prefix | process, byte-bounded LRU | `server_prompt_cache` (`tools/server/server-task.h:612`) |

There is **no disk-backed prompt cache**. `--cache-ram` (`-cram`) is RAM-only
(`common/common.h:616`, default `8192` MiB; flag at `common/arg.cpp:1705`). The only
disk paths are manual and operator-driven: `--slot-save-path` (`common/common.h:675`) and
the `/slots/save|restore` endpoints, which wrap `llama_state_seq_save_file` /
`llama_state_seq_load_file` (`server-context.cpp:2458`, `:2503`, `:5170`, `:5206`,
`server-context.cpp:4645`). Nothing is restored at start-up, nothing survives a restart.

## 2. Serialisation layer (what a disk format can build on)

Public API (`include/llama.h:897-928`):

```c
#define LLAMA_STATE_SEQ_FLAGS_NONE         0
#define LLAMA_STATE_SEQ_FLAGS_SWA_ONLY     1   // back-compat alias
#define LLAMA_STATE_SEQ_FLAGS_PARTIAL_ONLY 1   // partial states only (SWA, recurrent)
#define LLAMA_STATE_SEQ_FLAGS_ON_DEVICE    2   // keep data in device buffers

size_t llama_state_seq_get_size_ext(ctx, seq_id, flags);
size_t llama_state_seq_get_data_ext(ctx, dst, size, seq_id, flags);
size_t llama_state_seq_set_data_ext(ctx, src, size, dest_seq_id, flags);
```

Implementation notes that matter for us (`src/llama-context.cpp:2969-3040`):

* the blob is `io_magic (0xaf143cd8) ‖ seq_id ‖ memory->state_write(...)`. The magic is
  **not** a version: `LLAMA_SESSION_MAGIC/VERSION` only guards the whole-context session
  files (`state_load_file`, `:3040`). A disk cache must add its own versioned header.
* `seq_id` in the blob is *informational*: `set_data_ext(..., dest_seq_id, ...)` restores
  into any slot (`:3030-3032`), which is what lets one cached prefix be handed to
  different slots.
* `ON_DEVICE` keeps the state in device buffers per seq (`mem_storage[seq_id]`) and
  invalidates previously obtained device states for that seq — incompatible with
  persistence, must be excluded from the disk path.
* failures are reported as a short/zero return value, and exceptions are swallowed into
  `LLAMA_LOG_ERROR` + `return 0` (`:2987-2996`) — a caller cannot distinguish "too small
  buffer" from "corrupt state" without checking the size contract.

Per-seq filtering: `llama_kv_cache::state_write` (`src/llama-kv-cache.cpp:1969`) walks the
cell ring, keeps cells that are non-empty **and** carry `seq_id`, skips cells masked by
SWA (`is_masked_swa`, `:1998`), and emits `(cell_count, ranges)` + meta (`:2088`: per cell
`pos`, `n_seq_id`, seq ids) + data (`:2121`, bulk tensor ranges). The read side
(`state_read_meta`/`state_read_data`, `:2220`, `:2339`) **allocates fresh cells** for
`dest_seq_id` and copies into them, i.e. restore is an allocation + copy, and it can fail
if the cache cannot provide the cells — the return status must be checked, and restore
should be attempted only when the slot is otherwise empty.

### 2.1 `PARTIAL_ONLY` is the seam between the two kinds of state

For hybrid memory (`src/llama-memory-hybrid.cpp:189-201`):

```cpp
void llama_memory_hybrid::state_write(io, seq_id, flags) const {
    if (!(flags & PARTIAL_ONLY)) { mem_attn->state_write(io, seq_id, flags); }
    mem_recr->state_write(io, seq_id, flags);
}
```

i.e.

* `NONE` = **attention KV for that seq (grows with n) + recurrent state (constant per seq)**
  — self-sufficient, restorable;
* `PARTIAL_ONLY` = recurrent state (+ SWA sub-cache) **only** — *not* self-sufficient: it
  assumes the attention KV for the same prefix is still in the cache.

Same pattern for iSWA (`src/llama-kv-cache-iswa.cpp:259-273`), DSA-iSWA, DeepSeek-V4.
The server uses `PARTIAL_ONLY` for its checkpoints (§4) — which is precisely why
checkpoints today cannot survive a restart, and why a disk tier must use `NONE`.

### 2.2 Our models land in the hybrid path

`src/llama-model.cpp:2395-2470`: models without special-cased memory go through
`llm_arch_is_recurrent` / `llm_arch_is_hybrid`. `qwen35` / `qwen35moe` / `qwen3next` use
layer filters keyed on `hparams.is_recr(il)` (`:2441-2447`) and get
`llama_memory_hybrid_iswa` when `swa_type != NONE` else `llama_memory_hybrid` (`:2453-2480`).
Note `mtp_on_hybrid_qwen` (`:2412-2416`): an **MTP draft context over a Qwen hybrid gets a
plain attention KV cache**, so the target and draft contexts have *different* memory types.

Recurrent state size per sequence (`src/llama-hparams.cpp:183-229`,
`n_embd_r = (conv_kernel-1)*(ssm_inner + 2*group*state)`, `n_embd_s = state*inner`); for
`Qwen3.8-27B` (`qwen35`, 65 layers, `full_attention_interval=4`, `head_count_kv=4`,
`key_length=value_length=256`, `ssm.state_size=128`, `ssm.inner_size=6144`,
`ssm.group_count=16`, `conv_kernel=4`, both stored `GGML_TYPE_F32`,
`src/llama-model.cpp:2464-2469`):

| component | formula | size |
|---|---|---|
| recurrent `S` per recurrent layer | `128*6144*4 B` | 3.00 MiB |
| recurrent `R` per recurrent layer | `3*(6144+2*16*128)*4 B` | 0.12 MiB |
| **recurrent total (≈49 recurrent layers)** | | **≈ 153 MiB per sequence** |
| attention per token (16 attn layers) | `2*4*256*16 = 32768 elem` | 64 KiB (f16) / ≈ 34 KiB (`q8_0`) |

So a snapshot costs `≈153 MiB + 34 KiB·n_tokens` on this box — a *constant* term that
dominates short prompts and an *O(n)* term that dominates long ones. Any disk budget has
to be reasoned about in those two terms (see `design.md` §3.3).

## 3. The RAM prompt cache

`server_prompt` (`server-task.h:566`) = `server_tokens` + `list<common_prompt_checkpoint>`.
`server_prompt_data` (`:588`) = `{main, drft}` byte vectors (target + draft context
states). `server_prompt_cache` (`:612`) = `list<server_prompt_cache_state>` +
`limit_size` (bytes, from `--cache-ram`) + `limit_tokens`.

`alloc()` (`server-task.cpp:1711`):

1. if some cached prompt **fully contains** the new prompt → skip (already cached, `:1713-1720`);
2. sum checkpoint sizes; if `state_size_new > limit_size` → **skip the entry entirely**
   with a warning (`:1723-1735`);
3. drop cached prompts fully contained in the new one (`:1737-1748`);
4. `while (size() + new > limit) pop_front()` — LRU by insertion order (`:1750-1758`);
5. allocate; on `std::bad_alloc` **halve-ish the limit to 40 % of current size** and
   `update()`, giving up on this entry (`:1762-1776`).

`load()` (`server-task.cpp:1793`): score every entry by longest common prefix with the
incoming tokens; keep candidates with `f_keep = lcp/entry_len ≥ 0.25` ("don't trash large
prompts", `:1812-1815`); pick the entry maximising both `f_keep` and `f_sim`; restore with
`llama_state_seq_set_data_ext` for target and draft; then `prompt = std::move(it->prompt)`
and **`states.erase(it_best)`** (`:1856-1865`) — a cache entry is consumed on use, not
shared. Two slots cannot hit the same entry twice; there is no refcount and no pinning.

`update()` (`:1870`): trim to `limit_size`, then derive a dynamic token budget
`limit_tokens_cur = max(limit_tokens, limit_size/size_per_token)` and trim to it.

### 3.1 Where the server drives it

* construction: `server-context.cpp:1269-1277` (`cache_ram_mib != 0` → make the cache;
  `:1339` otherwise warn);
* slot selection: `get_free_slots()` → `ret->prompt_save(*prompt_cache)` →
  `ret->prompt_load(*prompt_cache, task.tokens)` → `prompt_cache->update()`
  (`:1543-1562`), i.e. the outgoing slot's state is snapshotted and the incoming prompt is
  matched against the cache **before the task starts**; only for `COMPLETION` tasks (`:1546`);
* `slot::prompt_save` (`:253-276`): `llama_state_seq_get_size_ext` then
  `llama_state_seq_get_data_ext` with `FLAGS_NONE` into the freshly allocated cache buffer
  — this is a synchronous, O(state size) memcpy **on the server thread**, inside task
  admission;
* `slot::prompt_clear` (`:288`): `mem.seq_rm(id, -1, -1)` + `prompt.clear()` — the only way
  memory is returned;
* idle-slot spilling: `--cache-idle-slots` (`common/common.h:613`, default `true`;
  `arg.cpp:1725`) saves every idle slot into the prompt cache when a new task starts, and
  with `kv_unified` also clears it (`[TAG_IDLE_SLOT_CLEAR]`, `:2320-2334`). This is the
  behaviour a disk tier extends: today "spill" means RAM, and the entry is gone when the
  process dies or the LRU trims it.

## 4. Context checkpoints (per-slot, RAM only)

`common_prompt_checkpoint` (`common/common.h:1137`): `n_tokens`, `id_task`,
`pos_min/pos_max`, `data_tgt`, `data_dft`, `data_spec` (speculative-decode side state);
implemented with the same `llama_state_seq_*_ext` calls
(`common/common.cpp:2254-2360`) — note `load_tgt`/`load_dft` `GGML_ABORT` on size
mismatch (`:2332`, `:2350`): a corrupt checkpoint is a crash, not a miss. That is fine in
RAM; it is disqualifying for anything read from disk.

Policy: `--ctx-checkpoints` (`-ctxcp`, default 32, `arg.cpp:1687`) and
`--checkpoint-min-step` (`-cms`, default 8192, `arg.cpp:1695`). `create_checkpoint()`
(`server-context.cpp:2209-2256`) prunes entries closer than `checkpoint_min_step` to an
earlier one, evicts oldest beyond `n_ctx_checkpoints`, and captures with
`LLAMA_STATE_SEQ_FLAGS_PARTIAL_ONLY` (`:2247`) — **recurrent/SWA only**, see §2.1.
Checkpoints are created *before* `llama_decode` of the current batch, so they do not
include it (`:3515-3519`). They are invalidated when `pos_max > pos_next` (`:3273-3279`)
and searched during prompt processing against `pos_min_thold = max(0, pos_next - n_swa -
(has_new_tokens ? 0 : 1))` (`:3180-3186`, restore at `:3234-3262`). Message spans
(`task.params.message_spans`, produced by `find_message_spans`, `:4193`; consumed at
`:3404`, `is_last_user_message` in `:3508-3512`) are what make a checkpoint land on a
conversation turn boundary — exactly the granularity we want on disk.

## 5. Summary of gaps

1. **Nothing survives a process exit.** For a harness that restarts/thrashes the server,
   every turn re-prefills the whole conversation.
2. **Nothing survives LRU pressure**: `alloc()` skips oversized entries, `update()` trims,
   and `load()` destroys the entry it uses.
3. **The cache is RAM-capped by the same budget as everything else** — on a 121 GiB box
   running an 87 GiB model, `--cache-ram` is a luxury we cannot afford at the size the
   workload needs (see `AGENTS.md`/`benchmark.md`: the 2026-08-27 eviction incident that
   forced `--cache-ram 8192 → 32768`).
4. **No crash consistency story** because there is no disk state at all.
5. **No cross-slot sharing** of a cached prefix (consume-on-use, LCP heuristic with a
   0.25 floor) and **no cross-process sharing**.
6. **Synchronous state extraction on the server thread** — fine for a 32 GiB budget in
   RAM, unacceptable for a disk write path that must not stall decode.
7. **`GGML_ABORT` on state size mismatch** — must become a recoverable miss for disk.
8. **`PARTIAL_ONLY` checkpoints are not self-sufficient for hybrid models**; a persistent
   snapshot must be `FLAGS_NONE` (or attention-segment + recurrent-state pair).
9. **No content addressing**: identity is "the token list of this slot", compared by LCP
   at query time (`O(cached_tokens)` per admission) rather than by hash lookup.
10. No visibility: `n_prompt_tokens_cache` (`server-context.cpp:1955`) is a per-slot token
    count; there are no hit/miss/evict/persist counters, which makes regressions invisible.
