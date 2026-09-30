# Block-level prefix sharing v0 — boundary-split blobs (block-sharing-v0)

Status: **design, awaiting Rain's walkthrough** — no code yet.
Inputs: `design.md` §3.1 (chain-hashed block keys, compat-key root), §10 phases 2/3;
`llamacpp-cache-audit.md` §2, §2.1 (state framing, `PARTIAL_ONLY` semantics);
`checkpoint-persist.md` (sidecar ledger — the trust anchor this reuses);
`implementation-plan.md` (upstream split items 3/7).

> **Problem (measured 2026-09-30, box0).** Three parallel sessions sharing an
> ~3.4k-token base hold 802.2 MiB of whole-slot KV blobs (3 × ~267.5 MiB) plus
> 2076.3 MiB of checkpoint sidecars — 2878 MiB against the 2048 MiB limit, LRU
> churning on every save. Every session blob re-stores the *same* base bytes:
> attention KV for positions `[0, B)` and the recurrent state folded at `B`.
> Cross-session cold starts pay full base prefill again (~10–35 s TTFT,
> `parallel_similar.py` A/B, `f32d013`).

> **Deliverable.** Blobs reference shared **blocks** on disk: one
> `base-attn[0,B)` + one `recr@B` per distinct prefix, session-local files hold
> only the tail `[B, n)`. A new session over a known base restores blocks and
> prefills only its tail. Blocks are keyed by a token-chained hash rooted in the
> compat key (`design.md` §3.1) — content addressing by token identity, not bytes.

---

## 1. The decision: fork-side partial save/restore (settled here, evidence below)

**Proxy-side blob assembly is infeasible for our hybrid models.** Evidence, all in-tree:

1. The only persistence surface the proxy can drive is `/slots/{id}?action=save`,
   which calls `llama_state_seq_save_file` — one whole-slot FLAGS_NONE file per save
   (`server-context.cpp:2661`). There is no endpoint or public API that emits
   attention-only bytes, or bytes restricted to a `pos` window.
2. The proxy therefore cannot *produce* `attn[0,B)` without reimplementing the
   section framing (`n_stream`, per-stream `cell_count`, meta rows, `ext` blobs) in
   Rust — framing that depends on live config (`kv_unified`, `n_stream`, draft ctx
   topology). Byte-offset surgery on whole blobs would additionally require the
   physical cell layout, which `state_write` walks at runtime (`llama-kv-cache.cpp:2055+`).
   Reimplementing that is the policy/semantics duplication the design corpus calls a
   bug farm, and a wrong guess is *silently wrong restore*, not an error.
3. `PARTIAL_ONLY` blobs (recurrent-only) already exist — they ride the `.ckpt`
   sidecar — but are explicitly *not restorable into an empty context*
   (audit §2.1; `design.md` §9). The proxy cannot combine recr-only bytes with
   anything to make a restorable state; assembly must respect the attn-then-recr
   consumption order (`llama-memory-hybrid.cpp:197-210`), i.e. happen **server-side**.

**Therefore:** the fork grows the *mechanism* (windowed attention section extraction
plus parts save/restore with server-side assembly, built on the `ckpt-persist`
ledger plumbing we already own); oLLM grows the *policy* (block registry, dedup,
refcount GC, when to seed vs restore). This matches the project's standing division —
"mechanism server-side, policy client-side" (the reason oLLM exists, #17107).

The windowed write/read filter is `implementation-plan.md`'s already-scheduled phase-3
upstream item (split review item 7: a `pos`-range filter in `llama_memory_*`). Building
it here on `ckpt-persist` keeps that PR shape intact.

## 2. Identity: token-chained block keys, compat-key root

Per `design.md` §3.1, computed **server-side** (only the server has post-template
tokens):

```
H[0]   = SHA256(compat_key_bytes)          // v0: the proxy's backend_sig fields; prod: §7 strict set
H[i+1] = SHA256(H[i] ‖ le64(n_tokens_chunk) ‖ chunk_tokens)
blockKey(B) = hex(H(B))[0:16]              // B = split boundary in tokens
```

Consequences, and the two sharp edges:

* **Same text prefix ⇒ same tokens ⇒ same key.** The proxy never guesses tokens.
  A session that seeds a base gets `blockKey` back in the save response and records
  `text-prefix-hash → blockKey` in its own registry. A *later* session whose base
  text matches hits the registry, restores blocks, and skips base prefill entirely.
  Distinct bases cold-seed separately and land on distinct keys. False sharing is
  unproducible because the key is computed over the tokens that were actually
  prefilled.
* **Key on tokens, not bytes.** GPU prefill is not guaranteed bit-deterministic
  (kernel reductions), so two sessions' `recr@B` bytes may differ in low bits.
  Content-addressing byte hashes would then never dedup. Keying on token chains
  makes the block *the same block by definition*; a re-save overwrites
  (last-publish-wins — the same rule the proxy already applies to blobs, and
  correct because either saved state is a faithful fold of `[0,B]`).

## 3. Mechanism: fork changes (branch `ckpt-persist`, additive)

> Naming: fork changes are **F-n** (mechanism, §3); build milestones are **M-n**
> (§7). The first draft of this doc used M-n for both — renumbered 2026-09-30 to
> kill the collision. E.g. "M-1 (fork)" milestone delivers F-1..F-4; M-2 is proxy;
> M-3 is silicon.

### F-1. `pos`-window filter on attention state (the one new primitive)

Add an optional `[p0, p1)` position filter alongside the existing seq/SWA filters in
`llama_kv_cache::state_write/state_read` (`llama-kv-cache.cpp:2055+/2125+`, filter at
`:2075-2086`), threaded through `llama_memory_i` + `llama_memory_hybrid` +
`llama_kv_cache_iswa`. Everything else (framing, meta rows, data chunks, `state_read`
cell allocation) is reused unchanged — a windowed section is a valid section of the
same format, minus the filtered cells. Restores of windows allocate fresh cells, so
physical layout never leaks (`state_read_meta:2333` + `seq_rm(dest,-1,-1)` at `:2344`).

Public surface: internal to the server path (v0 does not add public flags; the
`llama_state_seq_flags` enum stays untouched — the upstream PR can later expose it).

### F-2. Forced checkpoint at the current position

`POST /slots/{id}?action=checkpoint` — captures a real checkpoint at the slot's
current `n_past` through the existing `create_checkpoint` path
(`server-context.cpp:2398`), i.e. a genuine PARTIAL_ONLY fold at `B` with honest
`pos_min/pos_max` copied from the live memory. Needed because `B` = end of the seed
prompt is not necessarily a `spans.is_user_start` position, and pre-existing
checkpoints are evicted by the ledger policy. **We never forge ledger values**
(`checkpoint-persist.md`: `pos_min` is a rollback-validity promise).

### F-3. Parts save (`action=save&split=<boundary>`)

With an explicit boundary `B` (must equal a ledger checkpoint's `n_tokens`):

```
<name>.parts.json          manifest: {version, sig fields echo, B, tokens_sha, blocks:[...]}
<name>.blk-<key>.attn      attention section [0,B)   (window filter; target + draft)
<name>.blk-<key>.recr      recr snapshot @B          (the F-2 checkpoint's data_tgt/dft, verbatim)
<name>.tail.attn           attention section [B,n)    (window filter; target + draft)
<name>.ckpt                ledger, unchanged format (so non-split restore still works)
tokens [0,B)               live in blk.attn meta; tail token list in parts.json
```

The old whole-blob `action=save` remains the default and stays byte-compatible —
v0 is additive; oLLM's existing blob lifecycle (sidecar rules R1–R3) is unaffected.

### F-4. Parts restore (`action=restore_parts`)

Server-side assembly, in the exact consumption order (`hybrid::state_read`: attn then
recr): stream `blk.attn` → stream `blk.recr` → stream `tail.attn` into the same
`state_seq_load_file` machinery, then rebuild the ledger with the checkpoint record
stored **verbatim** in the manifest. Any missing part, magic/version mismatch, or
boundary not matching the recr snapshot ⇒ plain error ⇒ proxy falls back to seed
(cold prefill). **Never abort** — `GGML_ABORT` from `common_prompt_checkpoint::load_tgt`
(`common/common.cpp:2371`) is not reachable from this path because parts bytes go
through `state_seq_set_data_ext` framing, not `load_tgt` (same discipline as §6.2 of
`design.md`; v0 adds a CRC32C per part file into the manifest so corruption is a
counted miss, not garbage-in).

## 4. Policy: proxy changes (oLLM)

1. **Seed dance upgrade** (`seed.rs`): after `prefill_only` lands the slot at the base
   boundary, call `action=checkpoint` (F-2) then `action=save&split=<B>` (F-3). Response
   carries `blockKey`; proxy records `sha256(base_text_bytes) → {blockKey, B}` in a
   persisted registry (`<cache>/blocks/registry.json`, atomic rename-publish like blobs).
2. **Admission upgrade** (`lib.rs::chat_completions`): session miss + registry hit on the
   request's text prefix ⇒ `restore_parts` (block + own tail, or block only for a fresh
   session) instead of cold seed; forward with `id_slot` as today. Registry miss ⇒ today's
   behavior exactly (seed dance), now additionally publishing shared blocks.
3. **Block store**: `<cache>/blocks/<blockKey>.attn|recr`, refcounted by manifest
   references. GC = unlink blocks with zero live manifest references, LRU by mtime
   (mtime-LRU v0 convention; `enforce_limit` counts blocks + manifests + sessions).
   Sidecar rules extend: deleting a session manifest must drop its tail files and
   deref its blocks (reuse `sidecar_path`/`rename_sidecar` helpers, extended to the
   parts set — no hand-rolled renames).
4. **Quota honesty**: `GET /health` gains `blocks_n / blocks_bytes / session_bytes` so
   the win is visible and the churn state is measurable.

## 5. Why this is safe (inherited anchors, not new promises)

* **The checkpoint ledger remains the trust anchor** (`checkpoint-persist.md`): reuse
  after `restore_parts` happens only through a real checkpoint at `B`, captured live,
  persisted verbatim. No forged positions.
* **Compat gating unchanged**: parts live under blob names carrying `backend_sig`;
  a sig change strands them like today's blobs (correct, cold-prefill, documented).
* **Degradation ladder**: missing/corrupt part ⇒ counted miss ⇒ fall back to whole-blob
  restore if present ⇒ fall back to seed. Every rung is a correctness-preserving
  latency loss, same shape as the sidecar-absent degradation.
* **Write amplification**: per-turn saves publish only the tail window `[B,n)` — O(new
  tokens) for attention instead of O(n). The recr part changes only at new
  checkpoints, and those already ride the sidecar.
* **Known accepted v0 gaps** (documented, not hidden): proxy-side sig is 6 fields,
  not `design.md` §7 strict — fine while compat means *this* dev-stack model/config;
  revisit before any consumer. `/v1/completions` clients without a seed path keep
  today's exact-length behavior.

## 6. Expected win (predictions to be replaced by measurements, no claims)

From the 2026-09-30 baseline (blobs 802.2 MiB / 3 sessions; per blob ≈ 153 MiB recr +
≈ 114.5 MiB attn ≈ 3.4k tokens @ ~34 KiB/token):

* Disk: one shared base (≈ 267.5 MiB incl. recr@B block) + per-session tails instead
  of 3 whole copies ⇒ predicted ~60–70% blob reduction at N=3, improving with N.
* TTFT: session-over-known-base cold start = restore (~120 ms-class, E12h) + tail
  prefill, instead of full-base cold prefill (10–35 s, `f32d013` table).
* MTP draft (C9): draft blobs are plain-attention and carry the same base — the split
  applies to them too; F-3 saves the draft window alongside.

All predictions land in `benchmark.md`-style rows only after M-3 silicon runs.

## 7. Milestones (tests-first throughout)

* **M-0 (now)**: this doc; measured baseline above; the harness deltas.
* **M-1 (fork, RED first)**: new unit tests in
  `tools/server/tests/unit/test_slot_parts_save_restore.py`, tinygemma3 pattern from
  `test_slot_save_checkpoint_reuse.py` (`cache_ram=0`, `checkpoint_min_step=128`,
  native `/completion` with `id_slot` in body, seed pinned, `./tmp` only):
  1. `save&split` produces manifest + block parts (format contract) — red: endpoints 400 today.
  2. virgin slot `restore_parts` then request ⇒ `cache_n ≥ B` reuse — red today.
  3. restored continuation byte-identical to cold at `temperature 0` (correctness anchor).
  4. wrong-boundary / missing-part manifest ⇒ 4xx + slot usable cold (degradation ladder).
  5. whole-blob path unchanged (pins F-3 additivity; must stay green).
  Then implement F-1..F-4 in that order, CPU suite (`tests.sh`) between commits.
* **M-2 (proxy, RED first)**: `cargo test` with the scripted fake backend
  (`fake.rs`): registry persistence + atomic publish, refcount GC never deletes a
  referenced block, restore_parts-before-forward ordering, fallback ladder,
  sidecar/parts lifecycle. No fs outside temp, no network.
* **M-3 (silicon)**: rebuild HIP fork (**announced first** — prod dev stack restart
  ~3 min; kills `:1245`), run HANDOFF verification ritual in full, then A/B:
  `parallel_similar.py` disk (`du`) + TTFT table before/after; numbers land in
  `benchmark.md` rows + this doc.

## 8. oMLX evaluation + MyAgent consequence (revised 2026-09-30)

Rain: oLLM is confirmed core to the MyAgent stack. This fires the standing revisit
triggers (`checkpoint-persist.md` adoption note, HANDOFF settled decisions) and changes
two policy choices, not the mechanism.

What oMLX validates (from `omlx-cache-analysis.md`):
- Block sharing on their MLX stack = **sliceable attn blocks (fixed `block_size`) +
  boundary snapshots for non-sliceable state** (`boundary_snapshot_store.py`) — the
  exact two-term shape this doc assumes (attn window + recr fold). Our design.md §11
  rejected porting their pager; oMLX's own architecture confirms the *behaviour*
  transfers without it: chain-hashed block identity, per-boundary non-sliceable
  snapshots, atomic rename publish, index derived from files, startup quota convergence,
  honest queued-vs-persisted accounting, capped serial preload.
- Gaps we already out-design: no flock (single-process assumption), compat signature
  weaker than §7's strict set for our purposes, no abort-cooperative persistence.

What changes with MyAgent as core consumer:

1. **Fold quantization moves from phase-4 to precondition.** The recurrent fold is
   153 MiB/store on this box (audit §2.2, f32) and already ~72% of measured disk
   (2076/2878 MiB sidecars). Per-boundary auto-split is gated on making folds cheap;
   oMLX quantizes GDN state to bf16/int8/RHT for exactly this reason. Predicted
   (arithmetic from the f32 unit, to be measured): bf16 ≈ 76 MiB/fold, int8/RHT
   ≈ 38 MiB/fold. First step: bf16 for block-recr files only — restore correctness
   must be re-verified byte-vs-continuation at temperature 0 (tolerance-based, T2-style);
   this is a *lossy* state, honest only behind a measured quality gate.
2. **Strict §7 compat key lands before MyAgent ships, not "later".** A silently wrong
   restore in a shipped product is the failure mode the design calls the most dangerous;
   the 6-field `backend_sig` is sufficient only for a dev stack with no consumers.
   Adopt §7 fields + oMLX's discipline: learn lazily, mismatch ⇒ replace never reuse,
   foreign entries preserved, layout token versioned inside the key.
3. **Fixed-block chunking replaces one-boundary-per-seed as the target format**
   (post-v0): attn windows at `floor(n/B)` with B a multiple of checkpoint granularity,
   recr folds only at boundaries where a real checkpoint exists. Two sessions diverging
   mid-message still share whole blocks; seed-boundary-only sharing evaporates at the
   first divergence. This is the auto-split endgame with oMLX's chunking, not spans.
4. **The settled-decision revisit triggers all fire now**: rerun the thrash suite as a
   release gate before MyAgent targets oLLM; `cache_limit_mb` and LAN auth posture are
   blocking decisions, not loose ends.

Revised recommendation:
- **v0 unchanged** (seed-boundary split, whole-slot default intact): it is the
  measurement rig that produces the auto-split break-even numbers and validates the
  mechanism. Keep.
- **v0.1** (new, before MyAgent integration): fold quantization (bf16, gated on
  measured restoration quality) + chain-aware prune + strict §7 compat key. Auto-split
  becomes a flip of proxy policy on top: block files already carry `blk-<key>` names
  and a `blocks[]` manifest; fixed-B chunking is oLLM registry policy + F-1 filter
  reuse, no new fork surface.
- **Gate**: `parallel_similar.py` extended to a branching workload (fork off turn k,
  N ≥ 3, post-compaction chain) with disk + TTFT rows; this is the release gate and
  the auto-split go/no-go, using the break-even formula
  `shared_depth × ~34 KiB × N_sessions > kept_folds × fold_bytes`.

## 9. Questions for Rain (blockers, decide at walkthrough)

1. **Boundary source**: v0 = seed-boundary only (B = end of seeded base). Auto-split
   at every `is_user_start` span (design.md phase 2 shape) stays out. OK?
2. **Manifest format**: `.parts.json` + `blk-<key>.{attn,recr}` filenames pass
   `fs_validate_filename` ([A-Za-z0-9._-]+) — naming and one-block-per-boundary
   (not per-256-token chunk) acceptable for v0?
3. **v0 compat gate** = existing `backend_sig` (6 fields), §7 strict set deferred to
   upstream-facing work. OK while oLLM has no consumers?
4. **cache_limit_mb**: block sharing shrinks usage but 2048 MiB is being churned
   *today*. Raise is Rain's disk-budget call (HANDOFF loose end) — after M-3 numbers,
   or clean stale blobs now?
