# Answers to REVIEW-QUESTIONS.md

Answers to §6, plus the doc changes I accept from §1–§5 and §7. Reviewer was right on the
substantive points: **C1, C3, C7 and §2 are design bugs, not nits**, and the 300 GiB budget
moves write amplification from phase 2 to first order. Numbering follows the questions.

Decisions below are then applied to `design.md` / `implementation-plan.md` — see *Doc changes*
at the end for what still needs editing (not all of it is done yet; do not treat the design doc
as updated unless an item is marked **DONE**).

---

## 1. L2 as the sink for L1 eviction — **accepted, and it becomes the primary path**

Adopted. `server_prompt_cache` gains a sink callback invoked with the *dying*
`server_prompt_cache_state` **before** each of the four destroys (`alloc` oversized-skip
`server-task.cpp:1723-1735`, `alloc` make-room `:1750-1758`, `update()` trim `:1870-1885`,
`load()` consume `:1856-1865`). The handoff is a `std::move` of `server_prompt_data{main,drft}`
plus the token list and its chain hash — no `llama_context`, no device transfer, no extraction
dedup. Write sources are now named explicitly (this replaces `design.md` §5):

| id | source | cost | default |
|---|---|---|---|
| **W1** | L1 eviction | free (bytes already host-side) | on |
| **W2** | slot publish at idle, queue-empty only | device transfer (`llama_state_seq_get_data_ext`) | on |
| **W2p** | slot publish right after prefill | device transfer, **on the request's critical path** | **off** (see 5) |
| **W3** | shutdown drain, bounded | none (bytes already queued) | on, 2 s |

W1 is what makes the 8 GiB-L1 case work; it is also the path the upstream TODO in
`try_clear_idle_slots()` ("move slot to level 2 cache instead of removing", `:1567-1573`, C6)
is asking for — good anchor for the upstream pitch.

Note for the implementation: to hand W1 anything meaningful, the chain hash must be computed
when L1 *inserts*, not when L2 evicts. `server_prompt` gains a `prefix_chain` member, filled by
`prompt_save` once, and it travels with the entry through L1 and into L2.

## 2. Identity/tokens with `--cache-ram 0` — **slot owns a thin prefix record; hooks move out of L1**

Adopted option (b) with a twist. There is **one** prefix record, owned by the slot, shared with
L1 rather than duplicated:

* `server_slot` keeps `server_prefix prefix` = `{ tokens clone (or refcounted view), chain:
  vector<block_hash>, n_tokens }`. The chain is computed lazily, chunk-by-chunk, cached, and
  extended (never recomputed) as tokens are appended.
* `server_prompt` (L1's entry) holds the same data by value/copy — it already clones the tokens,
  so the marginal cost is 32 B per 256 tokens.
* **All L2 calls happen in `server_context_impl`, not inside `server_prompt_cache`**: admission
  (`get_free_slots`, next to the existing `prompt_save`/`prompt_load` block at `:1544-1562`),
  idle spill (`:2320-2334`), shutdown. Guarded by `cache_disk_mib != 0`, *never* by
  `prompt_cache != nullptr`. This is the C7 fix.
* Drive-by hardening that must ship with it, since we now depend on that path: `:2325`
  dereferences `*prompt_cache` unguarded (survives today only because `cache_idle_slots` is
  auto-disabled when `--cache-ram 0`, `:1338-1341`, and `prompt_save` early-returns on an empty
  prompt). Once L2 can stand alone, that block has to work with `prompt_cache == nullptr` —
  add the null guard, and make `cache_idle_slots` mean "spill to whichever tier exists" rather
  than "requires cache-ram" (also fixes the now-wrong help text, §7).

## 3. Non-destructive, shareable L2 reads — **yes, explicitly**

L2 entries are immutable content-addressed objects. A restore opens → verifies → reads →
closes and **does not unlink, does not mark consumed, does not require exclusive ownership**.
One file may be restored by many slots and by N restarts. Pinning exists only for in-flight
prefetch staging (§6.5) and, because the synchronous restore call itself holds no cross-thread
state, no long-lived pin is needed. The design doc now has to say the opposite of L1's
consume-on-use contract out loud — DONE in `design.md` §6.5/§6.6 wording below (to apply).

## 4. Publish granularity — **v1 is restart reuse only, stated loudly; message-span alignment moves ahead of segmentation**

Two things I had wrong:

* **Truncation is not available to us.** Aligning publishes to `B` boundaries by restoring a
  longer entry and dropping the tail is *incorrect for hybrid models*: the recurrent state has
  already folded tokens `[n', n)` into a fixed-size matrix, so "state at n" cannot be turned
  into "state at n'". (Attention cells could be trimmed with `seq_rm(p1)`; the recurrent part
  cannot.) So an entry is only reusable at exactly the `n` it was captured at.
* Therefore cross-session sharing requires **capturing at boundaries where state actually
  exists**, and the natural boundary is a message span — already computed
  (`find_message_spans`, `:4193`; consumed `:3404`) and already the basis of context
  checkpoints.

Consequences, accepted:

* **v1 scope statement (put it in the README and `--help`):** "restores a conversation after a
  restart or crash; does not yet share prefixes between concurrent sessions." Two hermes
  sessions with a common 6k system prompt each get their own copy in v1.
* **Phase reorder.** Message-span-aligned publish (old phase 3) becomes **phase 2**; segmented
  attn-tail + full-recurrent (old phase 2, needs the `pos`-window libllama change) becomes
  **phase 3**. Rationale: span alignment is a server-side-only change, it is what makes the
  shared system prompt one file instead of one copy per thread (space *and* hit rate), and it
  is the prerequisite for the sharing story the whole oMLX comparison is about. Segmentation is
  pure write-amplification optimisation and can wait — at 300 GiB it hurts, but it does not
  block correctness or the acceptance test.

## 5. Per-publish stall policy — **no extraction on a request's critical path**

Accepted with a hard rule: **extraction never runs while another slot can be decoded against,
and never runs between "task accepted" and "task launched".**

* W2 fires only when `queue_tasks.empty()` **and** no slot is processing — the box is idle, so
  the DtoH transfer competes with nothing. This is strictly better than today's
  `prompt_save`-inside-`get_free_slots` behaviour, and we should *not* add a second, more
  expensive copy of that stall.
* W2p (post-prefill, immediate durability) is **off by default** until T0.2 says what the
  transfer costs; if enabled, cap it by **bytes** (`--cache-disk-extract-max-mib`, default 256)
  and skip with a counter rather than stalling.
* The writer is additionally rate-limited by a byte token bucket
  (`--cache-disk-write-bps`, default unlimited, recommended 500 MiB/s on this box once measured),
  because at 300 GiB we will be writing a lot while weights sit in the same unified memory.
* If T0.2 comes back ugly (C1/C2 say it can be 16k backend calls in `v_trans` mode), W2p stays
  off and W1+W2+W3 carry the feature. That is the point of measuring first.

## 6. Directory layout — **accepted: one directory per unit**

`--cache-disk-dir` stays the **root** (default `~/.cache/llama-server/`); the effective
directory is `<root>/<unit>/`, where `<unit>` defaults to a sanitised model basename
(e.g. `qwen3.8-27b-ud-q4_k_xl`) and is overridable with `--cache-disk-unit NAME`. Each unit
directory has its own `--cache-disk` budget, its own `.lock`, its own scan, its own prune, and
its own `foreign_bytes` counter. Compat key remains the second line of defence (a unit dir is a
quota boundary, not a correctness boundary). Two units serving the same model share by passing
the same `--cache-disk-unit`. `--cache-disk-prune` operates on one unit directory only.

This also means the fork test container writes under `~/.cache/llama-fork/<unit>/` — separate
root, so tearing down an experiment is `rm -rf` of a directory that production never reads
(§10).

## 7. Durability model — **process-kill durable; power loss explicitly out of scope; no in-place header rewrite ever**

* **The promise, one sentence:** a *process* death — `SIGKILL`, OOM-killer, `podman kill`,
  `systemctl restart` — loses at most the publishes still in the writer queue (bounded by
  `--cache-disk-drain-ms` on the graceful path), and never corrupts an entry; **true power loss
  is out of scope** (a rootless box cannot test it honestly), and after power loss any queued
  write is simply absent.
* Mechanism unchanged: tmp → `fdatasync(file)` → `rename` → `fsync(dir)`.
* **§6.8 rewritten (the corrupting-crash path is gone):** the header is written **once**, at
  publish, and is **never rewritten in place**. LRU order is (i) in memory during the run and
  (ii) across restarts from **file mtime only**, refreshed by the maintenance thread with
  `utimensat` at most once per entry per 10 min (option **(a)**). `last_access` in the header is
  informational. Rejected (b) sidecars: doubles the inode and metadata traffic on btrfs to buy
  ±10 min of LRU precision we do not need. Rejected (c) dual headers: complexity for a field we
  no longer trust for correctness.
* **.tmp sweep simplified as proposed:** the sweep runs while holding the exclusive `flock`, so
  no other writer exists → delete **all** `*.tmp` unconditionally, no clock, no boot-time logic.
* Chaos suite: every T4 case is labelled `kill` (must pass) and the doc states that
  power-loss-class cases (`dm-delay`, `sysrq`) are not testable here and are not promised.

## 8. Restore pre-validation contract — **four checks, one of which we have to record ourselves**

Before ever calling `llama_state_seq_set_data_ext`, all of:

1. **envelope**: header magic + `fmt` version supported + header CRC + payload CRC32C;
2. **compat**: `compat_key_sha256` byte-equal to the live one (this is what makes C3's
   "structurally valid but semantically wrong" cases misses rather than aborts);
3. **size contract**: `payload_len == llama_state_seq_get_size_ext(ctx, seq, FLAGS_NONE)`
   measured *now*, on the running config. This is a strong structural check (same memory type,
   same tensor geometry) and costs one in-process call;
4. **capacity**: recorded `cell_count` (written at extraction, from the same call) against the
   live capacity, plus recurrent `rs` slot availability.

On (4), a correction to my own design: **there is no public `llama_memory_can_add` in this
tree** (`grep can_add include/llama.h src/llama-memory.h src/llama-kv-cache.h` → nothing), so
"check before restore" cannot be done from the public API today. Plan: v1 checks
`cell_count + llama_memory_ctx_used(...)`-equivalent is not available either, so we (i) keep a
conservative `--cache-disk-restore-headroom` (default 10 % of `n_ctx_seq`), (ii) treat a failed
restore as a miss + `prompt_clear()` (C4: `prompt_clear` is still required for the *slot's*
bookkeeping even though `state_read` does its own `seq_rm`, `llama-kv-cache.cpp:2226`), and
(iii) upstream a tiny `llama_memory_can_add(mem, n_cells)` helper as its own PR, then tighten
this check to use it. T8 gains the "valid CRC, wrong config" cases (bigger `n_tokens` than
`n_ctx_seq`, wrong `n_rs_seq`, target-only payload into a draft-configured server).

Also accepted from C5: recurrent restore forces `set_rs_idx(seq_id, 0)`
(`llama-memory-recurrent.cpp:849-851`) — record it as a known behaviour, and add a test that
restores the same slot twice in one process lifetime with `n_rs_seq != 0`.

## 9. MTP — **strict in v1: missing/mismatched draft ⇒ miss**, format reserves the option

`ctx_dft` is a single shared draft context assigned to every slot at construction
(`server-context.cpp:1210`, `:1099`), so "run this one request without spec decode" is not
available without real surgery. Therefore: an entry written by a server with a draft context
must contain a valid `state_dft` matching the draft compat key, else `reject_no_draft` (miss).
The entry header carries a `draft_present` flag bit, so target-only entries become possible in
a later phase (with per-request spec disable, or by treating a draft-less restore as valid only
when the server has no draft ctx) **without a format bump**. C9 accepted: T0.2 sizes `ctx_tgt`
and `ctx_dft` separately and the design doc stops implying one blob dominates.

## 10. Defaults — **flag default `10240`, units pass `307200`; root path unchanged**

* `--cache-disk` flag default stays **`10240`** (conservative, upstream-friendly; the fork does
  not silently write 300 GiB on someone's laptop). Both systemd units in `~/source/setup` pass
  `--cache-disk 307200`, and that is a setup-repo change for the principal, not for this repo.
* `--cache-disk-dir` default stays **`~/.cache/llama-server/`** — correct because the toolbox
  bind-mounts `/home/rain`, so it is host NVMe from inside the container (worth a comment in the
  unit file, since it is load-bearing).
* With §6 above: effective dir `~/.cache/llama-server/<unit>/`; fork test containers use the
  separate root `~/.cache/llama-fork/`.
* New in §4 of the design for 300 GiB (accepted): the quota counts **in-flight `*.tmp` bytes**
  (btrfs CoW ⇒ no shared extents ⇒ peak = budget + largest entry), is validated against
  `statvfs` free space at startup, is re-validated on `ENOSPC`, and is treated as *approximate*
  right after eviction (btrfs commits freed bytes with the transaction).
* `posix_fadvise(POSIX_FADV_DONTNEED)` after both write and read — accepted, and note why it is
  the right tool here: weights are `--load-mode none` (malloc'd), so we are competing with them
  for page cache and bandwidth, not for mmap pages.

## 11. Metrics — **yes: token-based, and honest about the conversion**

* `n_restored_tokens` = prompt tokens **not** prefilled (the primary success metric; it is
  measurable without any pp assumption).
* `saved_seconds_est = n_restored_tokens / pp_tts` where `pp_tts` comes from a new
  `--cache-disk-pp-tts` (default 0 = *unknown* → omit the estimate rather than invent it). T0.3
  produces the number; it goes into `~/source/setup/benchmark.md` and then into the units.
* Also: `publishes{source=evict|idle|shutdown}`, `extract_ms` / `restore_ms` histograms,
  `restored_bytes`, `written_bytes` (write-amp numerator) vs `ideal_bytes`,
  `rejects{reason=too_big|no_draft|compat|corrupt|locked|backpressure|no_cells}`,
  `staging_bytes`, `mode=rw|ro`, `foreign_entries|foreign_bytes`. Prometheus lines on `/metrics`
  alongside the existing `llamacpp:*` series.
* **Acceptance test = the crash that lost the thread** (T6, pass criterion made explicit):
  long hermes thread → `kill -9` → restart → first request's `prompt eval time` proportional to
  new tokens only, with `n_restored_tokens / prompt_tokens ≥ 0.9`, and the harness seeing no
  error. That criterion goes in the plan's DoD, not just in prose.

---

## Doc changes (marked inline; status as of `REVIEW-ANSWERS-2.md`)

1. **DONE** (commit `e043eb54c`) `design.md` §2 hook placement → `server_context_impl`,
   guarded by `cache_disk_mib != 0`, slot-owned `server_prefix`, non-destructive shareable reads.
2. **DONE** (`e043eb54c`) `design.md` §4/§1 + `HANDOFF.md` invariant 7 → device-transfer
   wording, `v_trans` call-count asymmetry, L1-eviction cheap path.
3. **DONE** (`e043eb54c`) `design.md` §6.8 mtime-only LRU / header-written-once; §6.2
   unconditional `.tmp` sweep.
4. **DONE** `design.md` §3.3 → `n_ctx_seq = n_ctx / n_parallel` nuance, per-conversation
   ≈ 8.5 GiB ceiling at `--parallel 3`, 300 GiB entry counts.
5. **DONE** `design.md` §6.10 + `--cache-disk-unit` → per-unit directories, symlink check on
   both components, prune/DELETE scoped to this server's unit.
6. **DONE** `design.md` §6.9 → tmp bytes in quota, `statvfs` at startup + on `ENOSPC`,
   `posix_fadvise(DONTNEED)`, approximate-after-eviction, chain-aware prune.
7. **DONE** `design.md` §3.2/§9 → `draft_present` as a correctness field, C9 draft-blob sizing,
   strict-miss policy (per-request spec disable does not exist).
8. **DONE** `design.md` §6.3/§6.4 → C4 (restore self-`seq_rm`s; `prompt_clear` is slot
   bookkeeping; order stated), C5 (`set_rs_idx(seq,0)` + double-restore test), pre-validation
   contract.
9. **DONE** `implementation-plan.md` → supersession banner, T0.0, hook placement fixed, five
   W1 destroy sites, W-ids + W4/W5, dedup invariant at the handoff, tier metrics, T6 tier
   criterion + T6b, T8b/T11/T12, phase reorder (span publishing = phase 2, segmentation =
   phase 3), `can_add` + null-guard as separate PRs, 307200 budget text.
10. **DONE** (`e043eb54c`) `README.md` → v1 scope sentence, read order, 300 GiB.
11. **TODO (code, not docs)** — upstream-facing PRs: `:2325` null-guard + `--cache-idle-slots`
    help text, `llama_memory_can_add`. Tracked in the plan's review split, items 3-4.

Also fixed in this pass: `HANDOFF.md` §3 item 3 (stale 10 GiB / "zero at 786k" text) and
`container/README.md` (F8: `--cache-disk 307200`, root `~/.cache/llama-fork/`).

One process note: I am treating your reordering proposal as accepted and will not re-litigate
§2/§4 — if a phase-2 discovery makes span-aligned publishing expensive (e.g. spans not being
available for `/completions` requests, where there is no chat template to span), say so and we
fall back to "B-boundary publish only where the request happens to end there", which degrades to
v1 behaviour rather than to something wrong.
