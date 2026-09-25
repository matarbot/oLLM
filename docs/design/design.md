# Design: disk-backed prompt cache for `llama-server` (`--cache-disk`)

Status: **design** — no code yet. Read `llamacpp-cache-audit.md` first (current mechanics,
line numbers at `5ea87ddad`), then `omlx-cache-analysis.md` for the behaviour we are
importing. This document specifies behaviour and invariants; `implementation-plan.md`
splits it into reviewable steps.

> **Problem.** Agentic harnesses (hermes, pi, Claude-Code-style tools) talk to
> `llama-server` over many turns, and they restart, crash, hot-reload and kill the server
> routinely. Today every such event costs a full re-prefill of the conversation, because
> `--cache-ram` is a process-lifetime RAM LRU (`llamacpp-cache-audit.md` §3) and no disk
> tier exists. On this box re-prefill is the dominant cost of a turn, and RAM is the
> scarcest resource (87 GiB of weights out of 121 GiB), so growing `--cache-ram` is not an
> answer — NVMe is.
>
> **Deliverable.** A durable, crash-tolerant, content-addressed second tier under the
> existing prompt cache, sized by `--cache-disk <MiB>` (default `10240`) at
> `--cache-disk-dir <dir>` (default `~/.cache/llama-server/`), with no new failure modes:
> every disk failure degrades to "cache miss", never to a wrong answer or a crash.

---

## 1. Goals and non-goals

Goals:

1. **Restart-equal-to-uptime.** After `systemctl --user restart llama-server` (or
   `kill -9`, OOM, a harness restart, a toolbox refresh), the first request of an existing
   conversation reuses the cached prefix instead of re-prefilling it.
2. **Thrash-tolerant.** Interrupting the server at *any* instruction boundary, including
   mid-write and mid-restore, must leave the cache either unchanged or consistently ahead.
   No index to lose, no torn entry readable, no stale entry restorable.
3. **Bounded and predictable.** Hard byte budget (`--cache-disk`), enforced before writing,
   convergent after a crash, and write amplification bounded and measurable.
4. **Off the hot path.** No disk I/O while a batch is decoding, and no state extraction on a
   request's critical path — extraction is a synchronous **device transfer**, not a memcpy
   (§4), and it runs at idle or on L1 eviction (whose bytes are already host-side).
5. **Honest.** Counters for hit/miss/persist/evict/corrupt/reject-reason, an endpoint to
   read them, and a `benchmark.md` row before/after.

Non-goals (v1):

* cross-process *write* sharing (one writer at a time; others read-only — §6.1);
* sharing cache entries between different models in one directory beyond a compatibility
  gate that simply ignores foreign entries (§7.3);
* KV quantisation/compression of the on-disk state (space is handled by eviction policy);
* replacing `--cache-ram` (L1 stays; disk is L2), or making `llama-server` a
  general-purpose KV database.

## 2. Architecture

```
                      ┌──────────────────────────────────────────────┐
 request ──▶ admit    │ server_prompt_cache (L1, RAM, --cache-ram)   │  existing
             slot     │   alloc / load / update                      │  unchanged semantics
                      └───────▲───────────────────────┬──────────────┘
                      lookup  │ (miss)                │ (write-behind, on publish)
                      ┌───────┴───────────────────────▼──────────────┐
                      │ server_disk_cache (L2)                       │  new
                      │  index: prefix_hash → {file, bytes, lru, ver}│
                      │  writer thread + prefetch thread             │
                      │  flock single-writer gate                    │
                      └───────────────────────────┬──────────────────┘
                                                  │ tmp → fsync → rename → dir fsync
                                        ~/.cache/llama-server/<xx>/<hash>.lkv
```

Placement: a new component `server_disk_cache` in `tools/server/server-cache-disk.{h,cpp}`,
owned by `server_context_impl` next to `prompt_cache` (`server-context.cpp:867`,
`:1269-1277`). **All L2 calls live in `server_context_impl`, never inside
`server_prompt_cache`**, and are guarded by `cache_disk_mib != 0` — never by
`prompt_cache != nullptr`. Reason: `--cache-ram 0` sets `prompt_cache = nullptr` (`:1269-1277`),
auto-disables `--cache-idle-slots` (`:1338-1341`) and skips the whole admission cache block via
`update_cache = update_cache && prompt_cache` (`:1544`), so any hook placed in L1 is dead code
in one of the two modes we must support (`REVIEW-ANSWERS.md` §2). Hook points: admission
(next to `prompt_save`/`prompt_load`, `:1543-1562`), idle spill (`:2320-2334`), L1's eviction
sink (W1), shutdown. Token/prefix identity is owned by a `server_prefix` record on the **slot**
(tokens + lazily-extended chunk-hash chain), shared with L1 rather than duplicated.

L1 remains authoritative for everything it holds; L2 is consulted when L1's `load()` finds
nothing better (`server-task.cpp:1793-1851`) and its reads are **non-destructive and
shareable** — the opposite of L1's consume-on-use contract (`:1856-1865`), because with a small
L1 many slots and many restarts hit one file.

Rationale for hooking the existing cache rather than adding a parallel path: the
hard parts (what to keep, when, with which tokens, draft-vs-target) are already solved
there, including message-span boundaries (`:3404`, `:4193`) and the idle-slot spill
(`[TAG_IDLE_SLOT_CLEAR]`, `:2320-2334`). Duplicating that policy is where bugs live.

## 3. Entry identity

### 3.1 Prefix chain hash

Adopt oMLX's parent-chained content hash (`omlx-cache-analysis.md` §1.1) instead of the
current LCP scan (`server-task.cpp:1795-1815`), because it turns lookup into an O(1) hash
probe, makes entries shareable/pinnable, and makes writes idempotent (which is what makes
aborts and crashes harmless, §6).

```
H[0]  = SHA256(compat_key_bytes)                       // "root", model+config identity
H[i+1]= SHA256(H[i] ‖ le64(n_tokens_in_chunk) ‖ chunk_tokens)
key(entry covering n tokens) = (H[n/B], n mod B ? H_tail : 0)
```

* chunks of `B = 256` tokens by default (`--cache-disk-block`, must be a multiple of the
  batch and of `checkpoint_min_step` granularity — v1 keeps `B` fixed and stores
  `n_tokens` alongside the final partial chunk's hash `H_tail`);
* the *compat key* (§7) is mixed in at the root, so a model or config change is one clean
  namespace switch rather than a migration;
* token identity is post-template, post-`add_bos` `server_tokens` — i.e. the same sequence
  the KV was computed for, including `LLAMA_TOKEN_NULL` media placeholders (entries
  containing media placeholders are **not persisted in v1**: the placeholder's KV comes
  from mtmd features we do not store — see also `:3392-3398` which notes the chunk is
  already in cache, and `do_checkpoint = do_checkpoint && !has_mtmd` at `:3506`).

Lookup at admission: hash the incoming prefix in `B`-steps, probe the index from longest
to shortest, take the longest hit that survives verification (§6.5). This subsumes the
`f_keep ≥ 0.25` heuristic: we always take the longest match, and the "don't trash large
prompts" concern disappears because entries are keyed by content, not owned by a slot.

### 3.2 Entry contents

One entry = one *restorable* state, i.e. everything needed to resume at token offset
`n` with an empty target context:

| field | source | note |
|---|---|---|
| `tokens_prefix_len` | `n` | |
| `state_tgt` | `llama_state_seq_get_data_ext(ctx_tgt, ..., LLAMA_STATE_SEQ_FLAGS_NONE)` | **full**: attn KV for the seq + recurrent state (§2.1 of the audit) |
| `state_dft` | same on `ctx_dft` when a draft ctx exists | see §9 on MTP |
| `tokens_sample` | first/last `k` tokens + count | cheap verification (§6.5) |
| `draft_present` (header bit) | whether `state_dft` is in this entry | **a correctness field, not metadata**: `ctx_dft` is one shared draft ctx for all slots (`server-context.cpp:1086`, assigned at `:1210`) and is *nulled* when spec init throws or `spec` is null (`:1197-1200`), so two runs with identical flags can differ in whether a draft exists. Missing/mismatched draft ⇒ **miss**, not a degraded restore (per-request spec disable does not exist); §9 |

Phase 2 adds segmented entries — attention-KV **tail** (`[p0, p1)` window) plus a full
recurrent state — to stop rewriting the whole attention blob every turn (§10). v1 ships
whole-seq snapshots only.

### 3.3 Sizing on this box (why the budget must be explicit)

From the audit (`§2.2`), per snapshot for Qwen3.8-27B with `-ctk q8_0 -ctv q8_0`:
`≈153 MiB` recurrent (constant) `+ ≈34 KiB/token` attention.

| conversation length | snapshot size | entries in 300 GiB (`--cache-disk 307200`) | entries in `--cache-disk 10240` (flag default) |
|---|---|---|---|
| 4k | ≈ 0.29 GiB | ~1060 | ~35 |
| 16k | ≈ 0.70 GiB | ~428 | ~14 |
| 32k | ≈ 1.25 GiB | ~240 | ~8 |
| 128k | ≈ 4.4 GiB | ~68 | ~2 |

**`n_ctx_seq`, not `n_ctx`, is the per-conversation ceiling (C8).** The 27B unit runs
`--ctx-size 786432 --parallel 3`, so `n_ctx_seq = 262144` — one conversation's attention tops
out at ≈ 262144 × 34 KiB ≈ **8.5 GiB**, not 26 GiB. A single conversation can never exceed that
regardless of the flag; the 26 GiB figure was `n_ctx` and is the *aggregate* ceiling across
slots. Entries above the per-conversation ceiling are impossible; the "entry too big" path still
exists for budgets smaller than one snapshot.

Consequences that shape the design: the recurrent term means *short* conversations are
cheap enough to always cache but never free; the linear term means long conversations must
be **segmented (phase 2)** or they will thrash the budget; and the "entry too big" path
must be a quiet, once-per-size warning, not the log spam `alloc()` produces today
(`server-task.cpp:1730-1734`).

## 4. Write path (write-behind, publish-atomic)

```
server thread (admission or post-prefill)         cache writer thread
──────────────────────────────────────────        ─────────────────────
should_store(prefix_hash, n)?                     (idle loop)
  ├─ policy filters (§5)                            dequeue Item{key, bytes, meta}
  ├─ in-flight / already-stored dedup               ├─ quota: reserve bytes (evict LRU first)
  ├─ extract bytes NOW (DtoH transfer, no file I/O) ├─ write <file>.tmp, fsync
  │    state_tgt, state_dft, tokens sample          ├─ rename → <file>, fsync(dir)
  ├─ take slot generation g (§6.3)                  ├─ index.insert(key, bytes, now)   ← publish
  └─ queue Item (bounded, 1 s timeout)              └─ L1 entry may now be dropped
      on full queue: drop the item, count "dropped_backpressure"
```

Invariants:

* **Only the server thread touches `llama_context`.** The writer thread receives owned
  `std::vector<uint8_t>` and performs plain file I/O. This mirrors oMLX's rule that the
  writer never calls GPU APIs (`omlx-cache-analysis.md` §2.1) and avoids any
  `ggml_backend` thread-affinity problem.
* **Extraction is a batch of synchronous device transfers, not a memcpy.**
  `llama_io_write_host` defers every `write_tensor` and issues one `ggml_backend_tensor_get`
  per (tensor, range) **in its destructor** (`src/llama-context.cpp:2566-2595`; restore is
  symmetric with `ggml_backend_tensor_set`, `:2613-2640`). With `-ngl -1` the KV is
  device-resident, so the cost is a DtoH transfer of the whole state — and it is
  **orders of magnitude** flag-dependent: in `v_trans` mode (`--flash-attn off`) the value loop
  issues one transfer per (layer × `n_embd_v_gqa` × range) ≈ 16k backend calls for one 16k
  snapshot, vs 32 on the `!v_trans` path (`src/llama-kv-cache.cpp:2154-2182` vs `:2183-2216`).
  Report `extract_ms`; get a real number from T0.2 before setting any policy. This is why
  extraction is never on a request's critical path (§5, W2p default off).
* **The cheap publish path is L1 eviction.** When `server_prompt_cache` drops an entry, the
  serialised host bytes already exist: handing them to L2 is a `std::vector<uint8_t>` move with
  no `llama_context` call and no device transfer. That is write source **W1** below and it is
  the primary capacity path (see `REVIEW-ANSWERS.md` §1).
* **Publish = atomic rename.** A reader either sees the old state of the world or the new
  one. Directory `fsync` after rename so the name survives power loss (oMLX
  `_fsync_parent_dir`, `paged_ssd_cache.py:853`).
* **Queued ≠ persisted** in every counter and log line (oMLX `stats.py:202-206`).
* **Bounded queue with backpressure**, sized from free RAM / entry size, `put` with a
  timeout, and on overflow *drop the disk write*, never block the request beyond the
  timeout (contrast oMLX which blocks up to 1 s; a dropped write is only lost speed here,
  because the next publish opportunity re-creates it).
* **No rewrite of what is already stored**: dedup on `key` (in-flight and index), so a
  harness that replays the same 20-turn conversation 20 times writes nothing new (this is
  also the write-amplification answer to §11 "harness thrash").

## 5. When to publish

Publish sources, all policy-gated (each is a small pure function so it can be unit-tested).
Naming per `REVIEW-ANSWERS.md` §1/§5 — **no extraction runs on a request's critical path**:

| id | trigger | cost | default |
|---|---|---|---|
| **W1** | **L1 eviction** — sink callback invoked before each of `server_prompt_cache`'s four destroys (`alloc` oversized-skip `:1723-1735`, `alloc` make-room `:1750-1758`, `update()` trim `:1870-1885`, `load()` consume `:1856-1865`) | free: bytes are already serialised host-side | on |
| **W2** | **slot publish at idle**, only when the task queue is empty and no slot is processing (alongside the existing idle spill, `:2320-2334`) | device transfer, competes with nothing | on |
| **W2p** | slot publish right after prefill (durability of the expensive part) | device transfer **on the request path** | **off** until T0.2; if on, byte-capped by `--cache-disk-extract-max-mib` |
| **W3** | `SIGTERM`/`SIGINT` drain of the writer queue (`--cache-disk-drain-ms`, default 2000) | none (already queued) | on |

W1 is the path that makes the `--cache-ram 8192` default viable (L1 holds ~11 entries of 16k
tokens, so L1 eviction, not restart, dominates the workload) and it matches the upstream TODO
in `try_clear_idle_slots()`: "move slot to level 2 cache instead of removing"
(`:1567-1573`). It also requires the chain hash to be computed when L1 *inserts* —
`server_prompt` gains a `prefix_chain` member that travels with the entry into L2.

De-duplication across triggers is by key, so the same state is never written twice.
A publish never happens *during* prompt processing except at the prefill boundary; the
existing per-slot checkpoints (`PARTIAL_ONLY`, `:2247`) stay in RAM — they are cheap and
useless across restarts (§2.1 audit). Note `--checkpoint-min-step` (8192) is a good
default for a *later* "persist at message boundary" feature (phase 3).

## 6. Crash and race analysis

This is the part the request is really about. Enumerated failure modes and the mechanism
that covers each.

### 6.1 Two processes on one cache directory

Realistic on this box: a `podman exec`'d `llama-server` orphaned after `SIGKILL` while
systemd starts a new one (an existing hazard in `AGENTS.md`), or a dev server on another
port pointed at the same path.

* At startup, `open(<path>/.lock, O_CREAT|O_RDWR)` + `flock(fd, LOCK_EX|LOCK_NB)`.
  * success → **read-write** mode;
  * `EWOULDBLOCK` → **read-only** mode: lookups and restores work, all stores are dropped
    (counted as `skipped_locked`, logged once at `INF`). Never evict, never unlink.
  * `--cache-disk-no-lock` (escape hatch for filesystems without POSIX locks; refuse to
    start with a clear message if the filesystem reports no `flock` support *and* the
    directory is not empty).
* The lock is held for the process lifetime and is released by the kernel on exit —
  including `SIGKILL` and container teardown, which is exactly the thrash case.
* Lock ordering rule: the lock file is a *gate*, never a resource acquired under another
  lock (no cycle possible).

### 6.2 Torn / half-written files (`kill -9` mid-write)

* writes go to `<hash>.<pid>.<seq>.tmp`; `fsync(file)` → `rename` → `fsync(dir)`;
* the entry header carries `payload_len` and a **CRC32C over the payload**; the reader
  verifies before `llama_state_seq_set_data_ext` is ever called;
* startup sweep deletes **all** `*.tmp` unconditionally (it runs while holding the exclusive
  `flock`, so no other writer can exist — no clock or boot-time logic needed) and any file
  failing a header parse, counting `corrupt_dropped`;
* a file whose payload is short/corrupt is treated as a *miss* and unlinked lazily — it
  can never reach `GGML_ABORT` (which is what `common_prompt_checkpoint::load_tgt` would do
  today at `common/common.cpp:2332` — we never route disk bytes through that path).

### 6.3 Slot state mutated while its write is in flight

The dangerous window is: extract bytes for slot `i` → queue → the slot gets a new task →
the queued item is published under a key that no longer describes slot `i`.

* **Content-addressing removes the whole class**: the key is derived from the *tokens*,
  captured together with the state under the server thread, so a published item always
  describes exactly the prefix it was extracted for. Wrong-slot publication is impossible
  because the restore path never reads slot state — it writes into a slot.
* Additionally each slot carries a `generation` counter bumped on every
  `prompt_clear`/task assignment; a queued item stores the generation it was taken at, and
  the "notify L1 that L2 has this now" callback is dropped if the generation changed. This
  only affects L1 bookkeeping, never correctness of on-disk data.
* `state_seq_get_data_ext` is called on the server thread, which is the same thread that
  runs `llama_decode`, so no decode can interleave with extraction. The writer thread is
  never given a pointer into KV buffers.

### 6.4 Restore racing slot use

* restore stays **synchronous inside slot admission**, before any batch for that task is
  submitted. Note the exact split (C4): `llama_state_seq_set_data`/`state_read` already does
  its own `seq_rm(dest_seq_id, -1, -1)` (`llama-kv-cache.cpp:2226`,
  `llama-memory-recurrent.cpp:962`), so we do **not** pre-clear the memory; what we *do* have
  to call is `slot::prompt_clear()` (`server-context.cpp:288`), which clears the **slot's**
  bookkeeping (`prompt.clear()` etc.) so the slot stops believing it holds tokens its memory no
  longer has. Order: `prompt_clear()` → pre-validate → restore → update slot prompt tokens.
* **Pre-validation contract** (C3): a payload that passes CRC but does not fit the running
  config can hit `GGML_ASSERT`s *inside* `set_data` after a successful slot search — an abort,
  not a miss. Before ever calling `llama_state_seq_set_data_ext`, all of: header magic + `fmt`
  + header CRC + payload CRC; `compat_key` equal to the live one; `payload_len ==
  llama_state_seq_get_size_ext(ctx, seq, FLAGS_NONE)` measured now; recorded `cell_count` vs
  live capacity (conservative `--cache-disk-restore-headroom`, default 10 % of `n_ctx_seq`,
  until a `llama_memory_can_add` helper exists — there is no public capacity query in this
  tree, so that helper is its own PR, see the plan). Any check fails ⇒ miss, never abort.
* Known behaviour to preserve (C5): recurrent restore unconditionally does
  `set_rs_idx(seq_id, 0)` (`llama-memory-recurrent.cpp:849-851`). A slot that restores more
  than once per lifetime with `n_rs_seq != 0` must be tested (T8: double restore).
* if the task is cancelled after restore but before decode, the normal cancel path runs
  (`prompt_clear`), leaving the slot empty; nothing else is needed because L2 published
  data is independent of slots.
* restore failure (short read, CRC ok but `set_data` returns a mismatched size, cell
  allocation failure at long context) ⇒ `prompt_clear()`, count `restore_failed{reason}`,
  fall through to a cold prefill. **Never** `GGML_ABORT`, never a partial restore visible
  to the task.

### 6.5 Prefetch racing everything

To keep disk latency off the decode path, matched entries are read in the background:

* a **prefetch** is scheduled when a task is *queued* (before it gets a slot): resolve the
  longest key, read the file into a staging buffer, verify CRC, and mark it ready;
* at admission, if the staging buffer is ready → restore from memory (fast); if not ready
  → either wait up to `--cache-disk-prefetch-wait-ms` (default 250) or miss;
* staging entries are keyed by `prefix_hash` and are **single-use**: consumed, dropped when
  the task is cancelled (`SERVER_TASK_TYPE_CANCEL`), or expired after
  `--cache-disk-staging-ttl-ms` (default 15000) — so a harness that queues 10 tasks and
  cancels 9 cannot leak 10 staging buffers. Total staging is byte-bounded
  (`--cache-disk-staging-mib`, default 4096) with LRU drop;
* prefetch never evicts, never writes, and its buffers are owned by exactly one thread at a
  time via `std::shared_ptr` handoff — the mutex is only ever taken briefly for the map,
  never across I/O.

This is oMLX's `preload_matched_blocks` idea (`paged_ssd_cache.py:4132`) with their
"serial, on the calling thread, capped by available budget" discipline replaced by an
explicit bounded staging pool, because we have no GPU-API constraint on the read side and
a hard requirement not to block admission.

### 6.6 Eviction racing a read, and the write-side dedup invariant

* all index mutations, publishes and unlinks happen on the single writer/maintenance
  thread;
* **dedup invariant (F3)**: L2 never writes a key it already holds or has in flight. The check
  is on the **final key** and happens **synchronously at the handoff**, before any bytes move —
  never deferred to the writer thread (a deferred check costs one 0.7 GiB copy and a queue
  slot per turn). In-memory `unordered_set` of stored keys (seeded by the startup scan) plus an
  in-flight set, updated at publish and prune; ~32 B/entry. Because the key encodes the unit,
  cross-unit duplicates are correctly *not* deduped. This is what makes the restore→L1→evict→W1
  cycle write nothing.
* a reader resolves a key → opens the file (or uses a staged buffer) **before** the
  writer can act, and ENOENT after resolution is a normal miss (`race_retries` counter);
* unlink is deferred: LRU candidates are moved to a `to_delete` list and unlinked on the
  next maintenance tick, after a grace period (`--cache-disk-grace-ms`, default 1000).
* an entry currently in-flight in a prefetch is pinned (`pin_count`) and never evicted
  while pinned.

### 6.7 Index loss / drift

There is no index file. The index is rebuilt at startup by scanning
`<path>/*/*.lkv` and reading each 256-byte header (`omlx-cache-analysis.md` §5), then
converging the quota before serving (`--cache-disk` may have been lowered between boots).
Scan must be header-only and is O(files): at 300 GiB and 0.7 GiB/entry that's ≈ 430 files,
so microseconds, and even 100k files is a sub-second start.

### 6.8 LRU persistence — header is written once, never rewritten

Rewriting a 256-byte `last_access` field in place inside a hundreds-of-MiB file is the only
place in this design where a crash could corrupt an otherwise perfect payload, so we do not do
it. **The header is written once, at publish, and never modified.** Cross-restart LRU order
comes from **file mtime only**; the maintenance thread refreshes mtime with `utimensat` at most
once per entry per 10 min (metadata only, btrfs-friendly, no corruption window). `last_access`
in the header is informational and never authoritative. `monotonic` clocks are never compared
across boots. (`REVIEW-ANSWERS.md` §7; supersedes the earlier "rewrite the header" text.)

### 6.9 Quota and disk-full

* reserve before write: `_enforce quota → evict LRU → write` (oMLX
  `_enforce_size_limit_for_new_block`, `paged_ssd_cache.py:4564`), so `--cache-disk` is a
  ceiling the process never exceeds by more than one in-flight entry;
* `ENOSPC`/`EDQUOT` on the underlying filesystem ⇒ disable writing for a backoff period
  (30 s, doubling to 10 min), keep reading, count `enospc`, and expose it in metrics — a
  full `/home` must never become a server error;
* if `--cache-disk` is larger than 50 % of free space at startup, log a `WRN` and clamp
  (oMLX auto-sizes at 10 % of capacity, `settings.py:334`; we clamp rather than auto-grow
  because the flag is explicit);
* the quota counts **in-flight `*.tmp` bytes** too: btrfs CoW gives no shared extents, so peak
  usage is budget + largest entry. The budget is validated against `statvfs` free space at
  startup and re-validated on every `ENOSPC`, and is treated as *approximate* right after an
  eviction (btrfs frees deleted bytes when the transaction commits, so post-eviction
  convergence is not instant);
* `posix_fadvise(POSIX_FADV_DONTNEED)` after each write and after each read: at hundreds of GiB
  of cache on a 121 GiB box the cache must not evict the weights' page cache. This is the right
  tool because the weights are `--load-mode none` (malloc'd) — we compete with them for RAM and
  bandwidth, not for mmap pages;
* **chain-aware prune (F1)**: under budget pressure, prefer deleting an entry whose same-chain
  descendant still exists — a shorter entry only serves shorter replays, and its longer
  descendant covers the full thread. Keep per chain roughly the newest entry plus one per
  doubling. This is what makes W1's free per-turn writes survivable at 300 GiB: without it a
  100k-token thread writes O(n²) bytes.

### 6.10 Per-unit directories, and model swap under a live cache directory

`--cache-disk-dir` is the **root** (default `~/.cache/llama-server/`); the effective
directory is `<root>/<unit>/`, where `<unit>` defaults to `<model-basename>-<sha8>` with
`sha8 = sha256(compat_key)[0:8]` and is overridable by `--cache-disk-unit`. The sha8 makes two
units serving the same file with different `--ctx-size`/`--parallel` get **separate** dirs —
correct, because their entries are incompatible anyway, and it stops one model's entries
squatting on another's quota. Each unit dir has its own `--cache-disk` budget, its own `.lock`,
its own scan and prune, its own `foreign_*` counters, and a `.unit` metadata file recording the
human-readable model path and the flag-derived identity fields.

Rules:

* the "refuse to follow a symlink at the leaf" check covers **both** `<root>` and
  `<root>/<unit>`;
* `--cache-disk-prune` and `DELETE /cache/disk` operate on **this server's unit only**. A
  server must never delete another server's entries; the operator escape hatch is
  `rm -rf <root>/<unit>/` while that server is stopped.

Model/config swap within a unit is handled by the compat key (§7): entries for other
configs stay on disk untouched (like oMLX's "skipped incompatible",
`paged_ssd_cache.py:2260-2265`), are excluded from the quota of the running config, and are
counted as `foreign_entries` / `foreign_bytes` in metrics.

## 7. Compatibility gate (`compat_key`)

The single most dangerous failure mode is restoring a state blob that was produced under
different runtime parameters — it produces *silently wrong* continuations, not a crash.
So an entry is only usable if its header's `compat_key` hashes to the root we compute at
startup. Fields (JSON, sorted keys, versioned):

```json
{
  "fmt": "llamacpp-disk-cache/1",
  "llama_build": "<LLAMA_BUILD_NUMBER+COMMIT>",
  "model": {"path_id": "<sha256 of resolved path>", "size": 16299331456,
             "mtime": 1769600000, "arch": "qwen35", "name": "Qwen3.8-27B",
             "n_layer": 65, "n_ctx_train": 262144, "n_kv": 4, "head_k": 256,
             "rope_type": "...", "full_attention_interval": 4,
             "ssm_state": 128, "ssm_inner": 6144, "conv_kernel": 4,
             "chat_template_sha256": "...", "tokenizer_hash": "..."},
  "kv": {"type_k": "q8_0", "type_v": "q8_0", "flash_attn": true,
          "v_trans": false, "swa_full": false, "kv_unified": true,
          "n_ctx_seq": 786432, "n_seq_max": 3, "n_rs_seq": -1, "n_swa": 0,
          "offload_kqv": true},
  "draft": {"present": true, "model_path_id": "...", "type": "mtp"},
  "pos": {"rope_scaling": "none", "yarn_*": null, "cache_type_k": "q8_0"}
}
```

Non-obvious inclusions, with reasons:

* **`flash_attn` / `v_trans`** — the attention memory is constructed with
  `attn_v_trans = !cparams.flash_attn` (`src/llama-model.cpp:2459`), so the *on-disk V
  layout changes* with `--flash-attn`. This alone would silently corrupt on our box, where
  every unit passes `--flash-attn on` (`AGENTS.md`).
* `type_k`/`type_v` — `-ctk/-ctv q8_0` here, f16 elsewhere: the blob stores raw rows.
* `n_seq_max`, `n_rs_seq` — recurrent state cells are laid out per seq
  (`src/llama-memory-recurrent.cpp:743-830`).
* `n_ctx_seq`, `kv_unified`, `swa_full`, `n_swa` — restore allocates cells and SWA masking
  changes which cells are even written (`src/llama-kv-cache.cpp:1996-2001`).
* `draft` — with `--spec-type draft-mtp`, the draft context is a *different memory type*
  (`mtp_on_hybrid_qwen`, audit §2.2), and the entry carries a second blob.
* `chat_template_sha256`, `tokenizer_hash` — token identity is what the prefix hash means;
  a template change invalidates everything, correctly.
* model `path_id + size + mtime` — cheap and catches the common "same name, new weights"
  case. **Deliberately not a content hash of the file**: 16–87 GiB, and the box cannot
  afford the read at every boot. Documented trade-off: two identical files at different
  paths get two namespaces (harmless), and a same-path same-size same-mtime rewrite is not
  detected (accepted; `--cache-disk-prune` fixes it).
* Strictness rule: **unknown or missing field ⇒ incompatible**, and a header whose `fmt`
  is newer than the binary's ⇒ ignored, never "best effort".

## 8. CLI, config, endpoints

Following the existing pattern (`common/arg.cpp:1705-1711`, `LLAMA_ARG_*` env,
`set_examples({LLAMA_EXAMPLE_SERVER})`):

| flag | default | meaning |
|---|---|---|
| `--cache-disk N` | `10240` | L2 budget in **MiB per unit directory** (§6.10); `0` disables L2 entirely; `-1` no limit (documented as dangerous). The box's units pass `307200`; the flag default stays conservative |
| `--cache-disk-dir DIR` | `~/.cache/llama-server/` | **root** directory; effective dir is `<root>/<unit>/`; a trailing `/` is accepted, `~` and `~/...` expanded, created with `0700` (missing ancestors with default permissions), refusing to follow a symlink at *either* component; **when `--cache-disk 0` is given the path is not created** |
| `--cache-disk-unit NAME` | `<model-basename>-<sha8(compat_key)>` | unit directory name (§6.10); two units serving one file with different configs get separate quotas; `.unit` metadata file records the human-readable identity |
| `--cache-disk-block N` | `256` | prefix hash chunk size in tokens |
| `--cache-disk-min-tokens N` | `512` | do not persist prefixes shorter than this |
| `--cache-disk-on LIST` | `evict,finish,cold,shutdown` | publish triggers by W-id (§5): `evict`=W1, `finish`=W4 (growth-gated, at generation end), `cold`=W3 (first publish of a conversation), `idle`=W2, `shutdown`=W5. **When `--cache-ram 0`, `finish` is force-added with a warning** — `evict` alone would be a silent no-op, since W1's only source is L1 |
| `--cache-disk-publish-stride N` | `2048` | W4 growth gate: publish at generation end only when the prefix grew ≥ this many tokens or ≥ ×2 in length (the crash-loss knob) |
| `--cache-disk-extract-max-mib N` | `256` | byte cap on on-path extraction (W3); larger ⇒ skip + count, never stall |
| `--cache-disk-write-bps N` | `0` | writer token bucket, 0 = unlimited (recommend measuring before setting) |
| `--cache-disk-restore-headroom N` | `10` | percent of `n_ctx_seq` reserved at restore, until a `llama_memory_can_add` helper exists (§6.4) |
| `--cache-disk-writer-threads N` | `1` | v1 accepts 1; API-shaped for later |
| `--cache-disk-prefetch-wait-ms N` | `250` | max admission stall waiting for a ready prefetch |
| `--cache-disk-staging-mib N` | `4096` | read-side staging budget |
| `--cache-disk-drain-ms N` | `2000` | shutdown drain budget before abandoning writes |
| `--cache-disk-prune` | — | drop foreign entries **of this server's unit only**, then exit |

Env: `LLAMA_ARG_CACHE_DISK`, `LLAMA_ARG_CACHE_DISK_DIR`, … (same convention).

Defaults chosen as requested. Note the default is **on**: `--cache-disk 10240` with the
default path is what the box's units will pass, and `--cache-disk 0` is the documented
off-switch, matching `--cache-ram 0` semantics (`server-context.cpp:1269`).

Endpoints (extend the existing `/metrics`, add):

* `GET /cache/disk` → JSON: budget, used, entries, hits/misses **and restored tokens/bytes
  with `tier=l1|l2`** (§F7), persisted bytes, `saved_prompt_tokens`, `saved_seconds_est`
  (tokens ÷ `--cache-disk-pp-tts`; omitted when that is 0 = unknown — never invented), rejects
  by reason (`too_big`, `no_draft`, `compat`, `corrupt`, `locked`, `backpressure`, `no_cells`),
  mode (`rw`/`ro`);
* `POST /cache/disk/flush` → drain writer queue (bounded);
* `DELETE /cache/disk` → drop **this server's unit's** entries (never another unit's).

## 9. Hybrid and speculative-decoding specifics

* Snapshot with `LLAMA_STATE_SEQ_FLAGS_NONE` so both attention and recurrent state are
  captured (audit §2.1). A `PARTIAL_ONLY` snapshot is *not* restorable into an empty
  context and must never be written to disk.
* **MTP draft context** (`--spec-type draft-mtp`, the current 27B unit): store `state_tgt`
  and `state_dft` in the same entry (mirroring `server_prompt_data{main,drft}`,
  `server-task.h:588`). If a draft context exists but its state is missing/mismatched in a
  found entry ⇒ treat as a miss (`reject_no_draft`), because `ctx_dft` is one shared draft ctx
  for all slots (`server-context.cpp:1086`, `:1210`) — per-request spec disable does not exist,
  so a degraded restore is not implementable without surgery. `data_spec` (eagle3-style side
  state, `common/common.h:1151`) is out of scope for v1: entries created while `spec_ckpt` is
  in use are skipped rather than persisted incompletely.
* **C9 — the draft blob is not a rounding error.** The MTP module is arch `qwen35` with a
  *plain attention* KV (`mtp_on_hybrid_qwen`, `src/llama-model.cpp:2411-2416`), so `state_dft`
  grows with `n` too. T0.2 sizes `ctx_tgt` and `ctx_dft` **separately**; the budget arithmetic
  in §3.3 is target-only and the draft term is added on top (measured, not estimated).
* Recurrent state restore consumes an `rs` slot; if `n_seq_max` is saturated, restore fails
  → cold path (`restore_failed{no_rs_slot}`).
* `qwen4exp` (Flash-Next) is the same shape (hybrid + indexer/PTE state, per
  `qwen4exp.attention.indexer.*`, `qwen4exp.ple.conv_kernel`). Extra non-sliceable state
  must show up in `state_write` for it to be persisted — verify empirically (phase 0 test
  "state round-trip equivalence"), because if `qwen4exp`'s indexer state is *not* in the
  seq state, restoring a snapshot silently loses it. This is the single highest-risk item
  in the design and it is a *test*, not an assumption.

## 10. Post-v1 extensions — phase order (reordered by `REVIEW-ANSWERS.md` §4)

**Phase 2: message-span aligned publishing (cross-session sharing).** The message spans are
already computed (`find_message_spans`, `server-context.cpp:4193`, consumed `:3404`) and are
the capture boundaries where state exists *exactly* at a shareable length. Publishing at span
boundaries makes a shared system prompt one file instead of one copy per thread — a space and
hit-rate win — and is server-side-only code. This is what fixes v1's stated limitation (no
cross-session sharing, because hybrid state cannot be truncated to a shorter prefix; the
recurrent state has already folded the tail in). Note: spans exist for chat requests; for
`/v1/completions` there is no template to span on, and publishing degrades gracefully to v1
behaviour (exact-length) rather than doing something wrong.

**Phase 3: segmented snapshots (write amplification).** Whole-seq snapshots cost `O(n)` bytes
and `O(n)` extraction per publish; a 100-turn conversation over 100k tokens writes ~44 GiB
total (chain-aware pruning, §6.9, contains this for a single thread but does not eliminate
it). Segmentation splits along the seam the memory layer already provides:

* `attn_tail` = attention-KV for cells with `pos ∈ [p0, p1)` — requires a new internal API
  (window on `llama_kv_cache::state_write`/`state_read`, which already iterates cell ranges
  and filters by seq and SWA mask: `src/llama-kv-cache.cpp:1990-2018`);
* `recr_full` = `PARTIAL_ONLY` state at `p1` (constant ~153 MiB here, ~50 MiB if stored in
  bf16 — oMLX quantises GDN state to int8/RHT codecs for exactly this reason,
  `omlx-cache-analysis.md` §1.2);
* an entry is a chain: `attn_tail[i]` files + the newest `recr_full`; restore replays
  oldest→newest (bounded by "keep at most `K` tails per conversation", default 8) and
  verifies the chain root;
* publishing then costs `O(new tokens) + O(153 MiB)` instead of `O(n)` — and the recurrent
  part is what forces a per-publish floor. A `recr_full`-only refresh (same tokens, newer
  state) is skipped.

## 11. Alternatives considered

* **Just raise `--cache-ram`.** No: process-lifetime, and RAM is the binding constraint on
  this box (`benchmark.md`, the 2026-08-27 eviction incident). Also does nothing for the
  restart case, which is the requested scenario.
* **Reuse `/slots/save` (`llama_state_seq_save_file`)** as the persistence primitive. It
  gives no content addressing, no verification, no versioning beyond the whole-context
  session magic, no async, and it is operator-driven; useful only as a *format reference*
  (we wrap `..._get_data_ext` and add our own header instead).
* **Paged/block KV cache inside llama.cpp (port oMLX's `PagedCacheManager`).** Rejected for
  v1: llama.cpp's memory layer is not paged, and re-implementing block tables + CoW +
  per-layer handlers would be a rewrite of `llama_memory_*`. The chain-hash idea transfers
  without the pager.
* **Persist at `llama_context` level (`llama_state_save_file`).** Saves the whole context,
  not per-seq: wrong granularity (can't restore one conversation into a busy server) and
  much larger.
* **External KV store (redis/sqlite) keyed by prefix hash.** Adds a dependency and a second
  lifetime problem; the file layout here is debugable with `ls` and survives the harness.
* **`mmap` the cache files and let the page cache be the cache.** Attractive for read cost
  (and worth revisiting for the read path), but it makes crash consistency and quota
  accounting implicit and hostile (`SIGBUS` on truncated mapping). Not v1.

## 12. Definition of done

1. `--cache-disk 10240 --cache-disk-dir ~/.cache/llama-server/` on the 27B unit;
   a hermes-style multi-turn conversation survives `kill -9` + restart with
   `prompt eval time` proportional to the **new** tokens only.
2. Chaos suite green (§plan §T4): `kill -9` during write, during restore, during eviction;
   two servers on one path (one goes `ro`); `ENOSPC` via a loop-backed fs; corrupt header
   injection; model swap; `--cache-disk` shrink across restart. No crash, no wrong output
   (byte-compare cold vs restored continuations at `temperature 0`), no quota breach.
3. Metrics endpoint reports `saved_prompt_tokens` and hit rates; `benchmark.md` gains
   before/after rows measured on a realistic 8k–32k hermes prompt (and a **proper long-prompt
   pp baseline**, which `benchmark.md` currently lacks — every recorded pp number there is a
   ~20-token microbenchmark and cannot be used to predict this win).
4. `--cache-disk 0` reproduces today's behaviour exactly (no directory created, no threads,
   no log noise) — the escape hatch must be real.
