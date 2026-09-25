# Implementation plan

Ordered so that each phase is independently reviewable, and nothing depends on an
unverified assumption. Effort is in "focused sessions", not FTE days.

> **Supersession:** where this file conflicts with `REVIEW-ANSWERS.md` / `REVIEW-ANSWERS-2.md`,
> the answers win (they settle the review round of 2026-08-28/29). The placement of L2 in
> `server_prompt_cache`, the `prefill,release,idle` trigger set, the 10 GiB budget text and the
> phase-2/3 order were all corrected there; the corresponding edits below are applied, but if
> you see another conflict, stop and read the two answers files.
> **Dev/acceptance target: the 27B unit** (`--parallel 3`, `n_ctx_seq = 262144`, MTP draft in
> every entry); Flash-Next is stopped for the build phase. Hermes prefix stability is assumed
> (principal, from experience) and verified by T0.0.

---

## Phase 0 — prerequisites (blockers, do first)

### 0.1 A build environment — **resolved: build a container, no dev toolbox**

Superseded plan (kept for the record): install a toolchain into a Fedora dev toolbox.
Better path found the same day: the toolbox project (`~/source/setup/amd-strix-halo-toolboxes`)
**ships the image recipes**, its `docs/building.md` documents local builds with
`--build-arg REPO/BRANCH`, and AMD publishes a gfx1151 dev meta-package
(`amdrocm-core-devel7.14-gfx1151`) which solves the missing-headers problem. So we compile
the fork in a builder stage and run it on kyuz0's own runtime image — production-identical
runtime, and recovery is deleting our tag. Recipe: [`container/README.md`](container/README.md).

Verified environment facts:

| env | compilers | cmake/ninja | git | ROCm dev headers |
|---|---|---|---|---|
| host | none | none | yes | none |
| runtime toolbox | `hipcc` only | none | none | none (no `rocblas.h`) |
| `fedora:44` + ROCm repo (builder stage) | gcc/g++ | yes | git-core | **yes** (`amdrocm-core-devel7.14-gfx1151`) |

Two hazards recorded in `HANDOFF.md` §2: a 32-job HIP build while a server holds ~110 GiB
of 121 GiB unified memory is an OOM/swap event (stop the server, or `-j 6`), and **both**
parity patches (`llama-grammar.patch`, `llama-cpp-25992-rocm-host-buffer.patch`) are absent
from this fork — build without them and the numbers are not comparable to `benchmark.md`.

Status: recipe written, **build not yet executed** (~1.5-2 GB download, 20-60 min compile).

### 0.2 Empirical facts we must not assume

Four short experiments, each producing a note in this directory (`findings/NN-*.md`):

* **T0.0 — hermes prefix stability (assumption, verify cheaply).** The whole design presumes
  the prompt is byte-stable from the top until compaction (principal's observation, untested).
  Log the first ~6k tokens' chunk hashes (one debug flag, server-side) for two consecutive
  hermes turns on the 27B unit and assert the common prefix is stable; also record where
  compaction cuts (a compaction starts a *new chain* — old entries simply stop being used and
  prune out; no correctness issue). **If the top of the prompt varies, no prefix hash ever
  matches and the feature is a no-op; this must be measured before any cache code is written.**

* **T0.1 — state round-trip equivalence (highest risk).** For `qwen35` (27B) *and*, if
  obtainable, a `qwen4exp` build: with a prompt of N tokens, take
  `llama_state_seq_get_data_ext(..., FLAGS_NONE)`; `seq_rm`; `set_data_ext`; decode the
  next token; compare against the unserialised run at `temperature 0`. Then the same after
  a process restart (write blob to a file, reload, restore). **If any part of the model's
  per-seq state is missing from `state_write` — e.g. `qwen4exp`'s indexer / PLE state — a
  restored snapshot will be silently wrong and the design must grow an explicit extra
  payload.** Do this before writing any cache code.
* **T0.2 — size vs time.** Measure `state_seq_get_size_ext` for n ∈ {1k, 8k, 32k, 128k}
  and extraction time, on the real model. Validates `design.md` §3.3's arithmetic
  (predicted ≈153 MiB + 34 KiB/token) and gives the memcpy cost that must stay off the hot
  path.
* **T0.3 — real long-prompt pp baseline.** `benchmark.md` has no usable prefill number
  (all recorded pp figures come from ~20-token prompts). Measure pp at 2k/8k/32k/64k
  tokens, cold, for both units, and add rows. Without this we cannot report the win.

## Phase 1 — L2 core, whole-seq snapshots (the valuable 70 %)

Deliverable: `--cache-disk 307200 --cache-disk-dir ~/.cache/llama-server/` (root; effective
dir `<root>/<unit>/`, §6.10) persists and restores whole-seq snapshots across restarts, with
the crash/consistency rules of `design.md` §6.

Files (new): `tools/server/server-cache-disk.h`, `server-cache-disk.cpp`,
`tools/server/server-cache-format.h` (header/CRC/compat key, no llama.cpp deps so it can be
unit-tested alone), `tests/test-cache-disk.cpp`.
Files (touched): `common/common.h` (`cache_disk_mib = 10240` flag default, `cache_disk_path`,
`cache_disk_unit`, plus the §8 knobs), `common/arg.cpp` (flags next to `-cram`, `:1705`;
**also fix the `--cache-idle-slots` help text**, which says "requires cache-ram" and becomes
wrong when L2 stands alone), `tools/server/server-task.h/.cpp` (**only** the W1 sink: each of
the **five** `server_prompt_cache` destroy sites — `alloc` subsumption `:1744`, `alloc`
make-room `:1756`, `load` consume `:1864` (hook **before** the `:1862` move), `update`
trim-size `:1875`, `update` trim-tokens `:1890` — invokes a sink callback with the dying entry
*before* the erase and taking ownership of it), `tools/server/server-context.cpp`
(construction next to `:1269-1277`, admission `:1543-1562`, idle spill `:2320-2334` —
**add the null guard around `*prompt_cache` at `:2325`** (latent: survives today only because
`cache_idle_slots` is auto-disabled at `:1341` when `--cache-ram 0`)), `tools/server/CMakeLists.txt`,
`tools/server/server-http.cpp` (`GET /cache/disk` etc.).

**Placement (settled, `REVIEW-ANSWERS.md` §2): all L2 call sites live in
`server_context_impl`, never inside `server_prompt_cache`, guarded by `cache_disk_mib != 0`
never `prompt_cache != nullptr`** — with `--cache-ram 0` the whole admission cache block is
skipped (`:1544`), so an L1-embedded hook is dead code in a mode we must support. Token /
chain identity is a `server_prefix` record **on the slot** (tokens + chunk-hash chain,
extended only by append at `n`, rebuilt from scratch on any other change — shift, truncation,
`prompt_clear`, reassignment), shared with L1 rather than duplicated; the chain is computed at
L1 insert time so the W1 handoff carries it for free.

Steps, in commit order:

1. **Format** — header (`magic 'LKD1'`, `fmt` version, `payload_len`, CRC32C, `n_tokens`,
   `compat_key_sha256`, `last_access`, json len + json), encode/decode, `crc32c`, and 20 unit
   tests (round-trip, truncation, bit flip, unknown field ⇒ incompatible). No I/O yet.
2. **Compat key** — `compat_key_from(params, model, vocab)` producing the `design.md` §7
   json. Unit-test that flipping any of `--flash-attn`, `-ctk`, `-ctv`, `--ctx-size`,
   `--parallel`, `--swa-full`, `--kv-unified`, `--spec-type`, `--rope-scaling`, chat
   template changes the key. **This test is the cheapest thing standing between us and
   silent wrongness.**
3. **Store primitives** — open/create dir tree (0700, symlink-refusal), fan-out
   `xx/`, atomic publish (tmp → fsync → rename → dir fsync), read+verify, unlink,
   header-only scan, quota reserve/evict (LRU with persisted `last_access`). Unit tests on
   `tmpfs`, including a `kill -9`-style crash test (`fork` + `_exit` mid-write, then reopen
   and assert no readable entry).
4. **`flock` gate** — rw/ro modes, `EWOULDBLOCK` handling, `--cache-disk-no-lock`. Test:
   two processes, second reports `mode=ro` and stores nothing.
5. **Writer thread** — bounded queue (bytes-bounded, `put` timeout), owned byte buffers,
   publish, counters (`persisted_bytes`, `queued_bytes`, `dropped_backpressure`), graceful
   drain with timeout. Test: extraction never performs syscalls (strace-count or a fake
   sink), queue overflow drops rather than blocks, drain honours the timeout.
6. **Index + lookup** — prefix chain hash (§3.1), `longest_match(tokens)`, pin/unref, and the
   **write-side dedup invariant** (F3): stored-key + in-flight sets, checked synchronously on
   the final key *at the handoff*, before bytes move — so a restore→L1→evict→W1 cycle writes
   nothing. Tests: chain correctness, model-A key never matches model-B, hit after restart,
   `--cache-disk` shrink converges at startup, shift-then-publish does not match the pre-shift
   entry (F4).
7. **Server glue, restore side** — in admission, after L1 `load()` finds nothing
   (`server-task.cpp:1793`), ask L2 (from `server_context_impl`); run the **pre-validation
   contract** (§6.4: CRCs, compat key, live `get_size_ext` equality, `cell_count` vs headroom)
   *before* `set_data`; `slot::prompt_clear()`; restore with `FLAGS_NONE`; on any failure
   `prompt_clear()` + count + cold path. Counters carry `tier=l1|l2` (F7). Correctness test:
   byte-identical `temperature 0` continuations, cold vs restored, at 3 prompt lengths, for
   `qwen35`.
8. **Server glue, store side** — the **W1 sink** (five destroy sites, §above) + W4 `finish`
   (generation end, growth-gated by `--cache-disk-publish-stride`, gated on `queue_tasks.empty()`
   and no other slot decoding) + W3 `cold` (first publish of a conversation, byte-capped) +
   W5 shutdown drain; policy functions injected; slot `generation` counter; `--cache-disk-on`
   with `evict,finish,cold,idle,shutdown` values and the `--cache-ram 0 ⇒ force finish` rule;
   `min_tokens`; mtmd-exclusion. Unit test: each of the five destroys hands off exactly one
   owned entry; a sink failure drops + counts, never throws back.

   **Slice split (settled):** A = finish/cold/shutdown + flags, all in `server-context.cpp`
   (+ `common.h`/`arg.cpp`, + `server_queue::queue_tasks_idle()`); no `server-task.cpp`
   changes. B = the W1 sink (five destroy sites) + `idle`. Slice A landed first because it
   is self-contained and unblocks the step-7 correctness test.

   **Slice A decisions (2026-09-02):**
   * Conversation identity is **per slot**: `server_slot::l2_published_n` (0 = cold pending),
     reset in `prompt_clear()` and when a non-continuation task is assigned
     (`launch_slot_with_task`); set on L2 restore success. v1-only, revisit with B.
   * `cell_count` = prefix **token count** (no public KV-cell API; conservative bound).
   * `--cache-disk-extract-max-mib` is **deferred** (no byte cap in slice A) - circle back
     before production use; the default 256 MiB would skip 27B q8_0 16k snapshots (~0.7 GiB).
   * `--cache-disk-min-tokens` default is **2048** (not the 512 in design §8).
   * `put()` timeout on the request path is 100 ms; a drop only loses speed (design §5).
   * `cold`/`finish` semantics: the first publish fires when either trigger is on; later
     publishes require `finish` and the growth gate (`+stride` tokens or ×2 length).
   * W4 gate: `queue_tasks_idle()` (new accessor: no task requiring processing; the
     no-op `NEXT_RESPONSE` loop poke is not counted - see below) and no other slot
     `is_processing()`.

   **Slice A live test (2026-09-05, Qwen3-8B Q4_K_M, 2 slots, 10 GiB budget):**
   3442-token prompt; publish at finish stores 3457 tokens / 258.3 MiB (prompt + 15
   generated). Phase 1 cold publish + SIGTERM drain wrote the entry; phase 2 (same
   prompt) did not re-publish; a fresh process loaded `entries=1` and restored the full
   conversation in ~10 ms (request 1.03 s vs 3.51 s cold prefill). Three bugs found and
   fixed during the test:
   1. `send_final_response` runs inside `server_queue::yield_to_queue()`, while the
      iteration's own `NEXT_RESPONSE` poke is still queued - a plain "queue empty" gate
      would therefore always block the publish. `queue_tasks_idle()` ignores
      `NEXT_RESPONSE` (its queue handler is a no-op; it adds no latency).
   2. The conversation-identity reset originally fired when the new task was shorter
      than the held prompt (a re-sent prefix) and when it was longer (a normal
      continuation after a restore). It now fires only when the common prefix is
      shorter than **both** - i.e. on a real divergence.
   3. After a F3 dedup (`put()` false because the key is `known()`),
      `l2_published_n` is still set, so the growth gate measures against the
      on-disk prefix instead of re-extracting at every finish.

   **Slice B decisions (2026-09-07):**
   * W1 sink: `server_prompt_cache::on_destroy` (a `std::function<bool(state &&)>`
     installed by `server_context_impl`); hooked at each destroy site (obsolete
     removal, make-room, size limit, token limit). No queue gate: the bytes are
     already host-side and the handoff is a move; `put()` timeout 10 ms, a drop
     only loses speed. Same `min_tokens` / mtmd gates as W3/W4.
   * W2 idle: three call sites - (1) admission, right after `prompt_save()` of the
     slot about to be overwritten (bytes still live); (2) the idle-spill block,
     before `prompt_clear()`; (3) the all-slots-idle branch of `update_slots()` -
     catches finishes whose W4 was gated by a busy queue and cancelled
     conversations. The task-type gate is null-safe via `task_prev` (a released
     slot publishes under its last task's type). Once published, the check is two
     integer compares per idle iteration.
   * Q5: `prompt_load()` reports whether it consumed an L1 entry (`consumed`
     out-param). A consume always starts a new conversation for the slot, so
     `l2_published_n` restarts at 0; the consumed prefix is then counted as
     published only if it is a prefix of the task **and** its key is already in
     the index.
   * Stale publish-state bug (found by the run-4 live test): the consume branch
     originally only raised `l2_published_n` on an index hit, leaving the previous
     occupant's value when the probe missed - a 4015-token conversation was gated
     against a stale 4792 and never published. The divergence reset in
     `launch_slot_with_task` cannot see this, because the consumed prefix *is* a
     prefix of the task. The reset on consume above is the fix.
   * `has_mtmd` currently excludes the whole conversation from L2; revisit for
     multimodal prompts (follow-up).
   * Live test (Qwen3-8B Q4_K_M, 2 slots, 10 GiB budget, 4 runs): W4 gated while a
     peer slot decodes; W1 dedup (prefix already on disk) and a visible evict write
     (4149 tok / 310 MiB); a fresh process restored the W1 entry in 12.6 ms; W2
     published a released slot (`task -1`) at all-idle; run 4 (L1 consume of a
     never-published prefix) ended in the expected cold publish at finish.
9. **Prefetch/staging** (§6.5) — background read on task queue, staging pool with TTL +
   cancel-drop, `prefetch-wait-ms`.
10. **Metrics + flags + docs** — `GET /cache/disk`, `POST /cache/disk/flush`,
    `DELETE /cache/disk`, `--cache-disk-prune`; `tools/server/README.md` section;
    `SRV_INF` one-liner at startup (`disk cache: mode=rw, path=…, budget=10.0 GiB,
    entries=…, restored_from_boot=…`) — because a silent cache is an unmeasurable cache.

## Phase 2 — message-span aligned publishing (cross-session sharing)

Persist checkpoints at message-span boundaries (spans: `:4193`, `:3404`), publish at the
lengths where state *exists*, and let a *different* slot/session resume from a shared
boundary. This is what fixes v1's stated limitation (no cross-session sharing — hybrid state
cannot be truncated, the recurrent state has folded the tail in) and makes a shared system
prompt one file instead of one copy per thread. Server-side only. `/v1/completions` has no
template to span on: publishing degrades to v1 exact-length behaviour, never to something
wrong.

## Phase 3 — segmented snapshots (write amplification)

`attn_tail` window API in `src/llama-memory*.h`/`llama-kv-cache.cpp` (a `pos` range filter
next to the existing seq/SWA filters, `:1990-2018`), `recr_full` via `PARTIAL_ONLY`, chain
entries with bounded tail count. Guardrail: a segmented restore must produce a state whose
`state_seq_get_size_ext(FLAGS_NONE)` matches a cold-run snapshot within tolerance, and the
`temperature 0` continuation test from P1.7 must still pass — that is what proves the
windowing is not silently dropping cells.

## Phase 4 — polish

`mmap` read path; optional bf16 downcast of recurrent state on write (oMLX quantises GDN
state further — `design.md` §10); multi-directory cache (`--cache-disk-dir a,b`);
upstream-ready refactor split (format + store as a small library with its own tests).

---

## Test plan

| id | what | how |
|---|---|---|
| T1 | format/compat-key/index/quota unit tests | `tests/test-cache-disk.cpp`, CPU-only build, `tmpfs` |
| T2 | restore correctness | golden `temperature 0` continuations, cold vs restored, several lengths, `qwen35` |
| T3 | quota | budget 1 GiB with 0.7 GiB entries: no breach beyond one in-flight entry, LRU order persists across restart |
| T4 | **chaos** (durability promise = process-kill; power loss is out of scope and labelled as such per test) | `kill -9` at: mid-extract, mid-write (before rename), after rename before index, mid-restore, mid-evict, during shutdown drain → assert (a) no crash, (b) cache either unchanged or consistent, (c) T2 still passes, (d) quota respected, (e) at most the in-flight publishes are lost |
| T5 | two writers | two `llama-server` on one path, different ports → second is `ro`; killing the first lets a third take `rw` without restart of the `ro` one being needed for correctness |
| T6 | harness thrash simulation (the acceptance test — the crash that lost this thread) | script that replays a 30-turn hermes conversation, `kill -9` after each turn, restart, assert `prompt eval time` ∝ new tokens and **`restored_tokens{tier=l2} ≥ 0.9 × prompt_tokens`** after every restart (tier-specific per F7: the L1-on run proves the integration, the L2 count proves the tier). With L1 on, W1's subsumption erase (`:1744`) is what makes it pass per-turn; note which source fired in the report |
| T6b | same as T6 with `--cache-ram 0` | the clean measurement: any hit is L2 by construction; proves W4 `finish` + W3 `cold` carry the feature without L1 |
| T7 | `ENOSPC` | loop-backed 200 MiB fs; server keeps serving, writing disabled with backoff, metrics show it |
| T8 | corruption | (a) bit flips / truncation → counted `corrupt`, treated as miss, no `GGML_ABORT`; (b) **structurally valid but semantically wrong** states (C3): valid CRC, `n_tokens` above `n_ctx_seq`, wrong `n_rs_seq`, target-only payload into a draft-configured server → all misses via the pre-validation contract, **no abort** |
| T8b | double restore (C5) | restore the same slot twice in one process lifetime with `n_rs_seq != 0` (recurrent restore forces `set_rs_idx(seq,0)`) → second restore still correct |
| T9 | config drift | start with `-ctk q8_0`, restart with `f16` ⇒ zero hits, zero writes to old namespace, `foreign_bytes` reported |
| T10 | off-switch | `--cache-disk 0` ⇒ no threads, no directory, byte-identical behaviour to `master` (diff the log surface) |
| T11 | tier matrix | `--cache-ram {32768, 8192, 0} × --cache-disk {0, N}`: cold correctness, restart reuse, idle-spill behaviour, and the `--cache-disk 0` byte-identical guarantee in every combination |
| T12 | write amplification | 30-turn replay, measure `written_bytes / ideal_bytes` (with chain-aware prune on and off); the number replaces the "< 1.2×" guess in the benchmark list |
| T13 | portability, **final gate before done** | Windows: server builds and runs with L2 cleanly disabled through the POSIX stub (no directory, no writer thread, byte-identical behaviour to `--cache-disk 0`); macOS: full store path runs (flock, 0700 dirs, dir fsync, atomic rename on APFS) with T1 + T2 + T10 green |

Upstream-facing requirements (if we ever PR this): no `GGML_ABORT` on user data, no new
mandatory files, CPU-only CI test (T1, T8, T9), documented flags in `tools/server/README.md`,
and `--cache-disk 0` default *upstream* even though our box defaults it on — a fork may
choose different defaults than upstream, but the off-switch must be provably clean.

## Benchmarks to record in `setup/benchmark.md`

1. cold prefill of an 8k / 32k prompt (baseline, no L2) — also fills the missing pp
   baseline (T0.3);
2. same prompt after a warm request, L2 on, **with** and **without** restart in between
   (the number that matters);
3. `tg` regression check: does the writer thread cost throughput during concurrent decode
   (run the existing 96-token microbenchmark while a 700 MiB publish is in flight);
4. `p99 admission latency` with and without `prefetch-wait-ms` (the added synchronous
   cost, `persist_extract_ms` + `restore_ms`);
5. SSD bytes written per conversation turn (write amplification) — measured by T12, reported
   with chain-aware prune on/off; the pre-measurement target was < 1.2× ideal, which is now
   expected to be violated by per-turn W1 writes — the prune policy is the fix, so report
   both.

## Risks

| risk | mitigation |
|---|---|
| seq state omits part of the hybrid/`qwen4exp` state ⇒ silently wrong restores | T0.1 **first**; if true, extend payload with an explicit extra section and gate on its presence in the compat key |
| extraction is a synchronous **device transfer**, not a memcpy (C1: `ggml_backend_tensor_get` per tensor/range in the io dtor; ~32 calls `!v_trans`, ~16k `v_trans`), and can stall other slots | extraction never runs on a request's critical path: W1 is free (bytes already host-side), W3/W4 are gated on `queue_tasks.empty()` + no other slot decoding, and byte-capped; measure `persist_extract_ms` per mode (T0.2 must produce the number in *both* `v_trans` modes before any policy is finalised) |
| long-context restores fail to allocate cells | `restore_failed{no_cells}` + cold path; consider "restore only if `n_tokens ≤ 0.8 × n_ctx_seq`" |
| write amplification at long context (phase 1 is `O(n)`) | phase 2; plus `--cache-disk-on` default excluding per-turn publishing when an entry of ≥ length exists |
| ROCm dev packages unavailable → can't build with HIP | CPU-only build covers T1/T8/T9; hybrid end-to-end needs the GPU, so escalate early (Phase 0.1 is first for that reason) |
| cache directory on an overlay/`/` inside the toolbox, not on the host NVMe | path default `~/.cache/llama-server/` is the *bind-mounted host* `/home/rain` in our units (`AGENTS.md`), verify with `stat -f` in a test; document that a container-local path makes the cache die with the container |
| two units (27B / Flash-Next) squat on each other's quota | per-unit directories (`<root>/<unit>/`, §6.10) with separate budgets; compat key remains the second line of defence *within* a unit; `--cache-disk-prune`/`DELETE /cache/disk` act on this server's unit only |
| someone points `--cache-disk-dir` at a tmpfs or an NFS mount | startup check: refuse non-local filesystems unless `--cache-disk-allow-remote`; log `statfs` fstype |
| POSIX-only store assumptions (flock, 0700 mkdir, dir fsync) leak into non-Linux platforms | T13 as the last step: Windows exercises the disabled stub, macOS runs the full store; audit every `sys/*` call per platform when it lands |

## Review split (for upstream-friendliness)

1. format + compat key + store (pure, testable, no llama.cpp state) — small PR;
2. `llama_state_seq_*` usage hardening: return-value discipline where `GGML_ABORT` exists
   (arguably upstream-worthy on its own);
3. **`llama_memory_can_add(mem, n_cells)` helper** — tiny libllama PR; there is no public
   capacity query in this tree (verified: `can_add` is absent from `include/llama.h`,
   `src/llama-memory.h`, `src/llama-kv-cache.h`), and the restore pre-validation contract (§6.4)
   wants it; until it lands, the `--cache-disk-restore-headroom` knob carries the load;
4. null-guard `*prompt_cache` at `server-context.cpp:2325` + the now-wrong
   `--cache-idle-slots` help text — small upstreamable fix, needed anyway because L2 must
   stand alone with `--cache-ram 0`;
5. L2 in `server_context_impl` + the W1 sink (five destroy sites in `server_prompt_cache`) +
   publish policy — the behaviour PR;
6. prefetch/staging — the concurrency PR;
7. (phase 3) the `pos`-window state API in `llama_memory_*` — libllama PR.
