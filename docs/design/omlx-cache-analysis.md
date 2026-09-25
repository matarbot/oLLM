# oMLX caching behaviour (reference analysis)

Source analysed: `github.com/jundot/omlx` @ clone of 2026-08-27 (`omlx/` Python package,
Apache-2.0). Purpose of this note: extract the *behaviour contract* of oMLX's tiered KV
cache — what it guarantees, where it can lose data, how it survives interrupted work —
so that an equivalent can be built on llama.cpp. Line numbers refer to that clone.

oMLX is Python + MLX, so nothing here is directly portable as code. What is portable is
the set of invariants, and they are unusually well thought out.

---

## 1. The three tiers

| tier | owner | granularity | eviction |
|---|---|---|---|
| GPU / device cache | `PagedCacheManager` (`cache/paged_cache.py:484`) | fixed-size block (`block_size` tokens) | refcount + free-list LRU |
| hot tier (host RAM) | `PagedSSDCacheManager._hot_cache` (`cache/paged_ssd_cache.py:1561`) | block | shared byte budget, LRU |
| cold tier (SSD) | `cache/paged_ssd_cache.py` | one safetensors file per block | LRU by `last_access`, byte-capped |

Blocks are vLLM-style: `CacheBlock` carries `ref_count`, `block_hash`, `token_count`,
`last_access` (`paged_cache.py:127`); free blocks live in an O(1) doubly linked list
(`FreeKVCacheBlockQueue`, `paged_cache.py:194`); hash → block lookup in
`BlockHashToBlockMap` (`paged_cache.py:378`). Sharing is by refcount, with copy-on-write
when a shared block is forked (`fork_block_table:1232`, `_cow_copy_block:1287`).

### 1.1 Content addressing is a parent-chained hash

`compute_block_hash(parent_hash, token_ids, extra_keys, model_name)`
(`paged_cache.py:78`) is SHA-256 over:

```
model_name ‖ (parent_hash or zeros) ‖ token_ids ‖ extra_keys
```

Consequences worth copying:

* a block's identity **encodes its whole prefix**, so a hit proves the prefix matched —
  no separate "conversation id" is needed;
* two requests sharing a 10k-token system prompt share those block hashes and therefore
  share both the RAM blocks and the SSD files;
* `model_name` in the digest isolates models inside one cache directory;
* block hashes are computed *before* the block is full, then re-hashed when full
  (`register_block_hash:1125`, `cache_full_blocks:956`) so partial blocks are never
  mistaken for complete ones.

### 1.2 Non-sliceable state is the hard case

Models like Qwen3-Next / Qwen3.5 / "qwen4exp" mix full-attention layers (sliceable,
grows with tokens) with Gated-DeltaNet/recurrent layers (state of size
`d_state × d_inner` per layer, **overwritten every token, position-independent**). oMLX
carries this distinction everywhere:

* `CacheType` / `type_handlers.py` classify each layer as sliceable (`KVCache`) or not
  (`ArraysCache`, `RotatingKVCache`, `PoolingCache`, `TurboQuantKVCache`, `CacheList`) —
  see `_CACHELIST_NON_SLICEABLE_SUB_CLASSES` (`paged_ssd_cache.py:372`).
* non-sliceable state **cannot** be chunked, so it is snapshotted whole at a *boundary*.
  That is `boundary_snapshot_store.py`: snapshots of non-sliceable layers are written to
  SSD **during** prefill, the device copy is freed immediately, and the snapshots are read
  back one block at a time when the request finishes (`boundary_snapshot_store.py:1-13`).
* GDN state gets its own sidecar files (format version `"2"`) with quantised encodings
  (`int8`, `rht_int8`, `rht_int16`, bf16 — `boundary_snapshot_store.py:53-64`) because the
  state is large
  (for our 27B: ~150 MiB/sequence, see `design.md` §3.3) and dominates the file set.
  Decode failures are *counted and refused*, never silently reinterpreted
  (`_validate_gdn_codec_metadata:1530`, `gdn_state_dequantizations` counter).

## 2. Write path

```
inference thread                          background writer thread
---------------                           ------------------------
save_block(block_hash, cache_data, ...)   _writer_loop()          (paged_ssd_cache.py:3073)
  ├─ adopt/verify layer signature         get((hash, tensors_raw, meta, path))
  ├─ dedup: index hit + compatible? → return      │
  ├─ incompatible indexed hit → forget_block      │
  ├─ hot cache / pending buffer hit? → return     ▼
  ├─ extract tensor BYTES (no mx ops)    _write_block_file()  (:2979)
  ├─ reserve hot-cache / queue budget      write tmp
  ├─ insert into pending-write buffer      fsync(file)
  └─ queue.put(..., timeout=1s)            os.rename(tmp, final)   ← publish point
                                           fsync(parent dir)   (:853, :980)
pending-write buffer serves reads until    mark hot-cache clean
the file is visible  (_pending_write_      clear pending buffer
 buffer_get:1976)                          (:3102-3110)
```

Points that matter:

1. **Tensor extraction happens on the inference thread; only raw bytes cross the
   thread boundary.** The writer thread performs no MLX/Metal calls at all, because
   touching GPU APIs off-thread deadlocks MLX (their comment cites MLX issues
   #978/#1040/#1106/#1437/#1558, `paged_ssd_cache.py:4216-4222`). Same discipline is
   needed for `ggml_backend_*_synchronize()` in llama.cpp.
2. **Publish = atomic rename.** Nothing ever reads a partially written block: readers
   only see final names. `os.rename` (`:2997`) for same-directory moves, `os.replace`
   (`:2421`, boundary detach) where overwrite is intended, always followed by
   `fsync` of the parent directory (`_fsync_parent_dir:853`) so the *name* survives
   power loss, not just the bytes.
3. **Read-your-writes.** A block that is queued but not yet on disk is served from the
   pending-write buffer, so the writer can never be raced by the next request
   (`_pending_write_buffer_get:1976`, `_promote_pending_write_to_hot_cache:2025`).
   Comment at `:2035`: "Keep the promoted copy dirty until its atomic SSD file is
   visible."
4. **Backpressure instead of unbounded queueing.** Queue depth is derived from host RAM
   and block size: target 10 % of RAM, hard cap 30 %, floor 32 entries, ceiling 256
   (`_PENDING_WRITES_TARGET_RAM_FRACTION` etc., `paged_ssd_cache.py:60-120`);
   `queue.put` uses a 1 s timeout so a stalled disk slows inference rather than eating
   RAM. Boundary snapshots use the same idea with byte reservations
   (`_MAX_PENDING_WRITES = 128`, `_DEFAULT_PENDING_MAX_BYTES = 512 MiB`,
   `_PENDING_RESERVATION_TIMEOUT_S = 2.0`, `boundary_snapshot_store.py:54-57`).
5. **Failure accounting distinguishes "on disk" from "queued".**
   `stats.py:202-206`: blocks are counted when `save_block` is called, and only
   incremented in the durable counter after the atomic rename in `_writer_loop`. Crash
   accounting is therefore honest: a queued-but-lost block is not reported as persisted.

## 3. Read path

`load_block` / `load_block_with_metadata` (`:3739`, `:3893`) try hot cache → pending
buffer → file. On a cold start with a long matching prefix,
`preload_matched_blocks` (`:4132`) pulls all matched-but-not-hot blocks into the hot
tier before decode begins, with three guards worth keeping:

* skip entirely if fewer than 4 blocks would be loaded (`:4165`, `:4192`);
* cap the preload by *available* hot-tier bytes so preloaded blocks cannot evict each
  other (`:4172-4191`);
* run it **serially on the calling thread** — a `ThreadPoolExecutor` version deadlocked
  against the inference stream (comment `:4216-4222`).

Reconstruction is defensive: any shape/dtype/signature surprise results in dropping the
block and re-prefilling (`_forget_incompatible_ssd_block`, `prefix_cache.py:482`), never
in a best-effort reinterpretation.

## 4. Compatibility gate (`cache_signature`)

Every persisted block carries a *compatibility signature* — `_cache_compat_signature`
(`paged_ssd_cache.py:252`), JSON with sorted keys over:

```
model_name, num_layers, block_size, layer_cache_types[], turboquant_kv_bits?,
cachelist_subtypes?, payload_layout, gdn_sidecar_state_dtype?
```

Behaviour around it is the part llama.cpp needs most:

* **learn it lazily**: the first `save_block` after a model load adopts the live
  signature if unset, then sweeps stale entries
  (`adopt_layer_signature_if_unset:4240` → `invalidate_stale_layer_signature:4334`);
* **a matching hash is not a hit unless the signature matches** (`save_block:3168-3190`);
  an indexed block with a bad signature is logged and replaced, not reused;
* per-block self-inspection: the signature is derived *from the block's own meta state*
  where possible (`_block_turboquant_bits:342`) so a stale manager expectation cannot
  poison the cache;
* **foreign blocks are preserved**: the startup scan indexes only blocks compatible
  with the loaded model and leaves others on disk (`_scan_existing_files:2238-2270`), so
  one shared cache directory can serve several models;
* layout changes are versioned *inside* the signature (`_PM_LAYOUT_TOKEN "@pm"`,
  `:390-396`) so that old cumulative blocks are invalidated rather than mis-spliced.

## 5. Startup recovery

`_scan_existing_files` (`:2238`) walks the fan-out subdirectories (`SUBDIR_CHARS`), reads
each file's **metadata only** (safetensors header, `_read_file_metadata:2886`), rebuilds
the in-memory LRU index, and then — importantly — if the directory already exceeds the
budget it converges immediately before serving requests (`:2296-2300`). GDN sidecars are
indexed from path + `stat` only, never by loading them (`_scan_existing_gdn_sidecars:2306`).

There is **no on-disk index file to lose**: the index is derived from the file set.
Combined with atomic publish, the worst case of a crash is a stray `*.tmp` and one
missing block.

## 6. Space management

`PagedSSDCacheIndex` (`:1056`) is a hash map + LRU order (`sort_lru_by_last_access:1095`,
`touch:1141`, `evict_until_size:1171`). Writes reserve space *before* allocating the
payload (`_enforce_size_limit_for_new_block:4564`, `_evict_tracked_until_size:4514`,
`_unlink_evicted:4662`). Defaults: `ssd_cache_max_size = "auto"` = **10 % of the
filesystem capacity** (`settings.py:334`, `get_ssd_capacity:107`), `ssd_cache_dir`
default `~/.omlx/cache` (`settings.py:333`), hot tier disabled by default
(`hot_cache_max_size = "0"`), `hot_cache_only` to run RAM-only.

Two shared budgets exist: the per-manager hot cache and a
`SharedHotCacheBudget` (`:1411`) that lets several model instances share one RAM budget
with `shrink_to` — relevant because llama.cpp may run a target + draft context.

## 7. Interrupted work: what oMLX guarantees

* **Aborted prefill.** Abort is a cooperative flag per uid, checked *between prefill
  chunks* (`scheduler.py:3623-3630`, `_pending_abort_ids:1794`); a chunk boundary abort
  raises `_PrefillAbortedError` (`:406`). Because blocks are committed per chunk and keyed
  by content, an aborted request leaves behind valid, reusable blocks — wasted work, never
  corrupt state.
* **Async store vs cleanup.** `store_cache` runs in a worker future; the request's uid is
  only released from the batch after the future completes
  (`_drain_pending_removes`/`_pending_async_removes`, `scheduler.py:2392-2460`). Boundary
  snapshot cleanup is **deferred to the same point** with the explicit comment: cleanup
  was moved out of `_cleanup_finished` "to avoid racing the worker's
  `boundary_snapshot_store.load()` calls with rmtree" (`:2449-2452`). This is exactly the
  migrate-to-cache race llama.cpp must not reproduce.
* **Per-request scratch is namespaced** (`_request_dir:843`,
  `_is_safe_snapshot_path:853`) and torn down as a unit (`cleanup_request:596`), with
  symlink-refusal on the root reset (`reset_boundary_snapshot_root:100`).
* **Cancelled writes** are handled by making the cancellation point an atomic rename
  (`boundary_snapshot_store.py:328`).
* **Shutdown** drains the writer queue (`close:4985`), and `save_block` will block on a
  full queue with a timeout instead of dropping silently.

What oMLX does *not* do: **no file locking**. It assumes one process owns the cache
directory. Two processes writing one directory (a real scenario for us: a crashed server
whose `podman exec`'d `llama-server` is orphaned while systemd starts a second one) would
interleave evictions. Our design adds an `flock` single-writer gate — see `design.md` §6.

## 8. Transfer list

Adopt (behaviour, not code):

1. parent-chained content hashes as the identity of a cache entry (§1.1);
2. "publish by atomic rename + dir fsync", "index is derived from files", "converge quota
   at startup" (§5, §6);
3. strict compatibility signature, learned lazily, mismatch ⇒ replace not reuse (§4);
4. read-your-writes buffer over the async writer, plus RAM-bounded queue with backpressure
   (§2.3, §2.4);
5. honest accounting: queued ≠ persisted (§2.5);
6. defer all cleanup of a request's cache artefacts until its async store finishes
   (§7.2);
7. separate treatment of sliceable vs non-sliceable (recurrent) state (§1.2) — mandatory
   for `qwen35` / `qwen4exp`;
8. cold-start preload of matched entries, capped and serial (§3).

Adapt or drop:

* per-block files: right for MLX's per-layer arrays, wrong for llama.cpp where a seq
  state is one opaque blob — we take per-*snapshot* files and, in phase 2, per-*segment*
  files (attention-only deltas + full recurrent state);
* safetensors: unnecessary, llama.cpp already has a serialiser (`llama_state_seq_*`);
* Python threading rules → replaced by "extract under the server thread, write on a worker"
  with ggml buffer-safety rules;
* "10 % of SSD" auto-sizing: our flag is explicit (`--cache-disk 10240`), see `design.md` §8.
