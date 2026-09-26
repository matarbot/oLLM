# HANDOFF — oLLM (as of 2026-09-26, ~09:30 CEST)

For the next agent picking this up. Read me, then `README.md` (product spec),
then `docs/design/README.md` (vendored deep design, historical framing).
Durable memory: deep-memory `projects/ollm-proxy.md` (the running log — this
handoff mirrors its latest state).

## Live state right now (verified at handoff time)

- backend `http://127.0.0.1:1245` — healthy. Runs in podman container
  **`llama-rocm2`** (NOT `llama-rocm-7.14` — that container died 2026-09-25,
  overlay corrupted, unrecoverable; do not try to start it).
  Start/restart: `bash ~/ollm-cache/start-backend.sh` (idempotent, recreates
  llama-rocm2 with correct binds if missing, polls health).
  Model: Qwen3.8-27B UD-IQ4_XS + MTP draft. API key: `~/ollm-cache/.backend_key`.
- proxy `http://127.0.0.1:1247` — healthy, debug binary from
  `~/source/oLLM/target/debug/ollm`, started by `~/ollm-cache/start-stack.sh`
  (start-stack also starts the backend if down; logs `~/ollm-cache/ollm.log`).
  `backend_sig=a67e59194299` — blobs in cache are keyed with this; it changes
  if the backend's model/config fingerprint changes (by design: stale blobs go inert).
- repo `~/source/oLLM`, remote `github.com/matarbot/oLLM`, main `c4ed07e`
  (T0/T1 refactor, pushed 2026-09-26), clean tree. `src/main.rs` split into
  `src/lib.rs` (app + `pub mod backend` / `seed`) + 17-line thin `main`;
  `src/backend.rs` (trait Backend + LlamaBackend), `src/fake.rs`, `src/seed.rs`
  (seed dance, unwired — E12 gate), 7 protocol tests. Edition 2024, zero
  `unsafe`. The running proxy (`:1247`) runs the NEW build (restarted
  2026-09-26 after the refactor; strict-write-test re-verified).
- Live backend note (2026-09-26, Matar's probe): `:1245` now runs with
  `--slot-save-path /home/rain/ollm-cache/slots` — save/restore is available
  on prod directly (old notes saying otherwise are stale).
- cache dir `~/ollm-cache/slots` (~1.3 GB, 9 blobs), cap `OLLM_CACHE_LIMIT_MB=2048`.

## What is DONE and verified (don't redo)

- E1 slot save/restore ground truth: save 16 ms/153 MB, restore 13 ms, KV +
  recurrent state intact; blob = whole-slot FLAGS_NONE, ~153 MB floor +
  ~34 KB/token. `save` works even while a slot is processing (don't assume idle-only).
- Persistence proof: hard backend kill → restore → recall (`KIWI-77`,
  `persistence-proof.sh`). Streaming + recall (`THETA-9`, `stream-test.sh`).
- Strict write rules (Rain's spec) implemented + verified (`4b50912`,
  `strict-write-test.sh` → recall `OMEGA-55`):
  1. write gate: lookup shared / publish exclusive (tokio RwLock)
  2. transactional publish: `.save` scratch → fsync file+dir → atomic rename
  3. dirty-slot registry: never steal/restore-over unpublished KV;
     write-on-eviction publishes victim before steal; failed publish ⇒ no steal
  4. compat gate: `backend_sig` embedded in blob filename
     (`session__<sig>.bin`, `88e1e71`)
- Streaming save race fixed: save is driven by pump-to-EOF in a spawned task,
  survives client disconnect. Never go back to slot-idle polling.
- Session key: `X-Session-Id` header (or prompt-hash fallback).

## Open work (priority order)

1. **Forest v1** — the next build. **Speced evening 2026-09-25: see
   "Forest-v1 decisions + gates" below — the testability refactor (T0/T1)
   is DONE (2026-09-26, `c4ed07e`); the current task is running E12
   (see "E12 execution plan" below), the physics gate the forest waits on.**
   Today: per-session blobs only. Target:
   prefix trees, longest-prefix-match at admission, leaf-first LRU eviction,
   promote hot spans to shared trunks. Key scheme already designed:
   `docs/design/design.md` §3 (chained hashes, root=compat_key). Hybrid
   constraint: a node can only be born where a save naturally ends
   (end-of-message span, or `n_predict=0` boundary) — recurrent state cannot
   be sliced mid-conversation. Chunk-boundary RAM sharing verified working
   (E7, build b10627), but cross-conversation *blob* restore mid-thread is
   silently wrong — only boundary snapshots are safe.
   E9 (restore-depth vs prefill-cost economics) belongs inside this.
2. **Queueing levers** (decided 2026-09-25, ladder-grounded, NOT yet coded):
   - `OLLM_MAX_INFLIGHT=1` admission semaphore (N=2 gives 7.3+7.3 tok/s,
     serial gives 21.7→21.7; MTP only pays off at N=1 — ladder `+64%`)
   - SJF forward order: restore-hit (~13 ms) before cold prefill (72–90 s @15–30k)
   - same-session barge-in priority for interrupt sequences
   - defer publish fsync/rename to GPU-idle windows
   Aggregate throughput is flat ~22 tok/s at any N — queueing reshapes
   fairness, never creates capacity. Warm ladder through proxy = phase 2.
3. **Fork-semantics tests** (Rain's scenarios): fork at 70%/95% (message-span
   boundaries — that's the ONLY legal save point on hybrid), original must
   stay clean, both lineages restore independently after restart, cost curve
   vs cut depth is a measurement, not a correctness question.
4. **Interrupt capture** — needs Rain at the desktop app: watch proxy logs
   while Rain interrupts a Hermes turn; encode what Hermes actually does
   (stop? cancel? full-history resend?) as oLLM's contract.
5. **Ops**: systemd units for backend+proxy on box0 (currently nohup —
   must self-heal at boot; yesterday the container death nearly orphaned
   everything); `cargo build --release`; LICENSE (README says TBD);
   finalize the session-key contract with how Hermes sends `X-Session-Id`.
6. **PARKED**: E8 flash-image warm-cache demo (Rain: "flagship is proven,
   UX polish, not for now"). Do NOT schedule it ahead of the forest.

## Forest-v1 decisions + gates (settled 2026-09-25 evening, Rain)

These decisions came out of a live design session and live ONLY here until
extracted into docs/design/. Do not re-litigate; extend by amendment.

### T0 — Testability refactor (DONE 2026-09-26, box0)
- `src/main.rs` (741 lines, zero tests) split into `src/lib.rs` + thin
  thin `main.rs` (17 lines). `src/backend.rs` = `trait Backend` + `LlamaBackend`
  (real reqwest impl) + `SlotState`/`ChatOutcome`. `src/fake.rs` = scripted
  fake with call recording. `src/seed.rs` = the seed-dance orchestrator
  (`SeedDance::<B,P>` + bag + `Publisher` trait, `FsPublisher` real /
  `MemoryPublisher` test) — NOT yet wired into the request path (waits on
  E12).
- Behavior preserved: after the refactor, `strict-write-test.sh` passed
  unchanged (turn1 publish → turn2 recall `OMEGA-55`; sig still
  `a67e59194299`). `cargo test` = 7/7 green, no GPU, no live fs, no
  run-date dependency.

### T1 — `trait Backend` contract (the fake's vocabulary)
| method | semantics |
|---|---|
| `slots()` | per-slot `id`, `is_processing`, `n_tokens` (position; on this build the field is the slot's `n_prompt_tokens` — verified live 2026-09-26) |
| `prefill_only(tokens)` | forward prompt with `n_predict=0`; slot ends at position N |
| `save_slot(id, name)` | llama.cpp `?action=save` scratch → caller fsyncs+renames (R2) |
| `restore_slot(id, name)` | `?action=restore`; 404 ⇒ cold-prefill fallback |
| `chat(body)` | forward `/v1/chat/completions`, stream pass-through |

Protocol tests against the fake (ALL WRITTEN 2026-09-26 in `seed.rs`, 7/7 green — the last two remain as the forest's acceptance tests):
- miss ⇒ `prefill_only` strictly before `save_slot`; never save unconfirmed position
- never `restore_slot` onto a dirty slot (R3 as an assertion, not log-reading)
- (box0 amendment 2026-09-25, EXECUTED 2026-09-26) position-trust check —
  **ran live, caught a real bug**: the running backend's `/slots` has NO
  `n_tokens` field at all; the slot's true context position is
  `n_prompt_tokens` (verified: an `n_predict=0` prefill of a 59-token system
  prompt leaves `n_prompt_tokens == 59` on an idle slot;
  `n_prompt_tokens_processed` is 0 when idle and is NOT the position). The
  fake initially masked this by reporting `n_tokens` — exactly the gap the
  check exists for. `LlamaBackend::slots()` fixed to read
  `n_prompt_tokens` (see `SlotState` doc); the fake keeps `n_tokens` as the
  contract name. "Never save an unconfirmed position" now rests on a field
  verified live, not on the fake's word.
- second sight of same system-hash ⇒ `restore`, ZERO `prefill_only` calls
- `save_slot` fails mid-seed ⇒ bag stays uncached, full request still served
  normally, no dirty-slot leak
- cap pressure ⇒ eviction yields only childless nodes; protected after tips

### T2 — Forest-v1 decisions
- **System-prompt bag**: hash of `messages[role=="system"]` TEXT (recognition
  is structural — OpenAI wire format, no template parsing, no tokenization at
  the proxy). Bag entry states: `uncached → cached` (+ `failed`). (The
  `queued` state was dropped 2026-09-25: seed-on-first-sight seeds in the
  same admission, so nothing ever sits in a queue.)
- **Seed-on-first-sight** (Rain's decision, not second-time): at the miss,
  in the same admission — `prefill_only(system)` → confirm `n_tokens == N`
  and `is_processing == false` via `slots()` → `save_slot` → bag=`cached` →
  forward the full request to the same slot (RAM prefix-match serves the
  delta). Rationale: the first request pays that prefill regardless; a
  second-time speculative seed pays it AGAIN (measured 72–90 s @ 15–30k,
  IQ4_XS batch-1) and races the GPU. Second-time speculative seed only as
  GPU-idle fallback when the first window was missed.
- **Trunk = hash collision, not a category.** No special system-prompt code
  paths in the forest proper; the bag is only the seeding policy. Byte-
  equality is the contract: MyAgent's static prompt head ships byte-stable
  per release; volatile injections (dates, memory context) go in the tail so
  they branch off the trunk instead of breaking it.
- **Eviction rules** (supersedes v0 flat mtime when the forest lands):
  1. Structural invariant: only childless nodes are ever evictable —
     children's keys derive from parents' hashes; evicting an interior node
     orphans every blob beneath it. This IS leaf-first, stated correctly.
  2. Stickiness by length (fixed 153 MB floor vs linear prefill cost):
     childless AND `n_tokens >= OLLM_STICKY_MIN_TOKENS` ⇒ PROTECTED with a
     30-day grace (`last_access`-pegged, ~one prompt-release cycle).
     Threshold lives in config (a PARTIAL_ONLY-style tail trim later shifts
     the whole curve stickier); pin the real value in E9 — back-of-envelope
     crossing point is somewhere around 1–2k tokens, cite E9 when measured.
  3. Everything else: pure mtime LRU among eligible (tips, non-sticky
     childless, grace-expired trunks). Length never enters the score; it
     enters via the protected class.
  4. HARD-CAP OVERRIDE (the suicide-pact clause): grace is a preference,
     never a cap violation. Under soft-cap pressure evict eligible first;
     if still over cap, evict protected worst-first (oldest mtime) with a
     log line. A month of accumulated prompt versions must never blow
     `OLLM_CACHE_LIMIT_MB`.

### T3 — E12: the physics gate (forest waits on this; runs on real hardware)
Fork-at-boundary equivalence on the hybrid (Qwen3.8-27B IQ4_XS, box0):
1. seed: `n_predict=0` prefill of the boot system prompt → save → seed blob
2. cold arm: same system prompt + 3 DIFFERENT user messages, temp 0, fresh
   slots — record continuations
3. warm arm: backend restart, restore seed blob, fire the same 3 divergent
   continuations
4. token-level diff of arms. Identical ⇒ boundary restore is safe and
   prefix sharing (forest, factory seed) is green-lit. Divergent ⇒ we learn
   exactly where the boundary assumption breaks — worth more than another
   week building on it. Run in-process variant first, then post-restart.
5. (box0 amendment 2026-09-25) MTP isolation: run arms with spec decoding
   OFF (pure greedy) first — the shipping config has `--spec-type
   draft-mtp`, so a divergence with MTP on is ambiguous (restore vs. spec
   path). Only after greedy arms are byte-identical, optionally re-run with
   MTP on to characterize the spec path separately.
   (Matar addendum 2026-09-26, confirmed via relay): the MTP-on re-run is
   not just speed characterization — it is the DIRECT test of recurrent-state
   rewind under speculative rollback. On the hybrid, the target verifies k
   draft tokens in one forward pass, folding the GDN recurrent state forward
   across all k; on rejection it must rewind to exactly the last accepted
   token (KV truncates trivially, recurrent rewind needs checkpoint machinery
   in the unified cache). If greedy passes and MTP-on diverges, the diagnosis
   is the spec rewind, immediately and specifically. Companion check:
   blob-size comparison must be seed-save vs seed-save (n_predict=0) with MTP
   toggled, same prompt + slot config, or sizes aren't comparable; if sizes
   are equal, grep the blob header for a draft identifier if the format
   exposes one.
Companion cheap test (same session decision): seed/cold equality —
split-prefill (system-only, then full) vs monolithic, same continuation,
temp 0 → identical bytes (proves `n_predict=0` saves land where the template
says). Once both pass on the shipping model/config, freeze responses as
golden fixtures; the fake replays them and CI never needs the box again
until `backend_sig` rotates — test schedule mirrors compat-key schedule.

### E12 execution plan (decided 2026-09-26, Rain — CURRENT TASK)

The next session's job is running E12. Everything below is settled; do not
re-litigate, extend by amendment.

- **Side-port server, never the prod one.** Rain's decision: E12 runs on a
  SECOND llama-server on **port 1246** (the scaffold convention). `:1245`
  (prod, serving the box0 bot + oLLM proxy) is NOT touched — no restart
  window, no save/restore of the bot's own conversation slot, no stranded
  bot. For different configurations: **shut down the test server and relaunch
  it with the new flags** (Rain's instruction) — never two test configs at
  once.
- **Test server = prod flags minus MTP** for the greedy arms:
  `--model /home/rain/models/lucebox-qwen/Qwen3.8-27B-UD-IQ4_XS.gguf
  --alias qwen3.8 --port 1246 --ctx-size 786432 --parallel 3
  --n-gpu-layers -1 --cache-type-k q8_0 --cache-type-v q8_0
  --cache-ram 32768 --api-key "$(cat ~/ollm-cache/.backend_key)"
  --flash-attn on --slot-save-path /home/rain/ollm-cache/e12/slots`
  (NO `--spec-type draft-mtp` / no draft model). Optional MTP-on re-run:
  relaunch the test server WITH the two MTP flags (`--spec-type draft-mtp
  --spec-draft-model /home/rain/models/qwen3.8-27b/MTP/mtp-Qwen3.8-27B-Q4_0.gguf
  --spec-draft-n-max 3`). Run inside `llama-rocm2` via `podman exec -d` with
  the redirect INSIDE the container's shell (Pitfalls).
- **Memory check first**: 128 GiB UMA, ~41 GiB available as of 2026-09-26
  morning; a second 27B IQ4_XS instance needs ~16 GiB + ctx. Check
  `free -g` before launching the test server; if it doesn't fit, E12 waits —
  do not evict prod.
- **Gates, in order** (all on the test server):
  1. **Blob-size check (hard gate, from Matar 2026-09-26)**: seed-save
     (`n_predict=0`) with MTP-OFF vs seed-save with MTP-ON, same prompt +
     slot config. Identical sizes ⇒ blob layout is invariant to the spec
     flag (confirms "draft not in blob"); MTP-on larger ⇒ draft IS folded in,
     the Q3 reasoning is wrong, re-plan the MTP-on arm. If equal, grep the
     blob header for a draft identifier if the format exposes one.
  2. **Greedy E12 proper** (T3 steps 1–4, MTP off): seed prefill → save;
     cold arm (fresh slots, 3 divergent user msgs, temp 0); warm arm
     (restart test server, restore seed blob, same 3 continuations);
     token-level diff.
  3. **Companion split-vs-monolithic test** (cheap, same session).
  4. **Optional MTP-on re-run**: the direct test of recurrent-state rewind
     under speculative rollback (see Matar addendum above).
  5. **Freeze golden fixtures** to `~/ollm-cache/e12/fixtures/a67e59194299/`
     (Rain-confirmed path convention: `fixtures/<backend_sig>/{seed.bin,
     cold.jsonl, warm.jsonl}`), box0-owned; pxl consumes, never writes.
- **Slot mechanics** (Matar 2026-09-26, grounded): reuse is the unified RAM
  prompt cache (block-aligned prefix match), no pinning; after a test-server
  restart the next request is a full cold prefill unless a disk restore
  serves it. Save/restore route is `/slots/{id}?action=save|restore`
  (E1-verified; singular `/slot/` 404s). Re-read `/slots` immediately before
  any save and identify slots by token count, never by remembered number.
  If a restored slot's KV doesn't prefix-match a fresh request (conv-UUID
  reset), `timings.prompt_ms` disambiguates: hundreds of ms = hit, minutes =
  reset-and-prefill.
- **Blob math for the bot's own ~99k conversation** (extrapolation from E1's
  16 ms/153 MB, ~34 KB/tok): ≈ 3.5 GB, save ~0.5 s, restore ~0.3 s. Only
  relevant if we ever revisit saving the prod conversation slot — the
  side-port decision makes this unnecessary.
- **What a divergence means**: identical ⇒ boundary restore is safe, forest
  + factory seed green-lit. Divergent ⇒ record exactly where (first divergent
  token index, which arm, greedy vs MTP-on) — that is the finding, not a
  failure of the run.
- **E7 note**: chunk-boundary RAM sharing was verified on build b10627;
  current build is b10664 (`e70802a01`). E7 does NOT carry over
  automatically (build_sig is deliberately NOT in oLLM's backend_sig — the
  sig covers model_path/total_slots/n_ctx/chat_template/add_bos_token/
  bos_token only) — the in-process arm of E12 on b10664 re-establishes it
  essentially free.
- **Relay status**: as of 2026-09-26 ~09:30 the relay delivers bot-to-bot
  replies into Matar's open Bot Chat (in-turn delivery), not into our
  `replies/` waiter — Matar's E12 answers arrived manually. Don't burn time
  re-deriving this; if the relay is still misbehaving, check Desktop cookie
  state (skill pitfall).

## Pitfalls (learned the expensive way — all live)

- `pkill -f llama-server` / `-f start-backend` over ssh matches the ssh
  command itself → kills your session. Use `[l]lama-server`, `pkill -x`,
  or kill by explicit pid.
- Never send nested-quote curl/json through `ssh '...'` — write a script
  file, scp it, run `bash file.sh`. Quoting has eaten hours.
- `podman exec -d ... > file` redirects on the HOST, not in the container.
  Wrap in `sh -c '... > /home/rain/...'` (bind mount makes it visible both sides).
- Container at uid 1000 cannot read `/home/rain` bind (perms) — llama-rocm2
  runs as root for exactly this reason. `--user 1000:1000` breaks
  `--slot-save-path` with EACCES.
- `usage.cached_tokens` is unreliable on this server. Verify KV reuse via
  proxy logs (restore/publish lines) + backend `timings`, not usage fields.
- qwen3.8 emits `reasoning_content` before `content`. Empty-looking answers
  are usually reasoning eating `max_tokens` — check reasoning_content before
  declaring a cache failure. Content decode ~97–102 chars/s; cold prefill
  72–90 s at 15–30k. All perf claims must cite a measured number + workload
  (an unverified "30+ tok/s" was corrected in the vault — don't reintroduce it).
- Health `cache_mb` showed 4217 (>2048 cap) once mid-write — suspected
  double-count of scratch `.save` during enforcement. Cache-pressure test
  is owed; enforcement path is untested under real pressure.
- Backend restart changes nothing in the blob keys unless config fingerprint
  changes (sig stayed `a67e59194299` across this session's rebuild —
  same image, same flags).

## Settled decisions (don't relitigate; revisit trigger on record)

- **No vLLM.** Disk prefix caching = GPU-RAM pool (same UMA competition
  Rain rejected for `--cache-ram`); gfx1151 fast paths incomplete; batch-1
  device ⇒ vLLM's edge inert; shipping quant is IQ4_XS GGUF. Revisit ONLY
  if a measured gfx1151 vLLM row beats llama.cpp+MTP at batch-1 on this
  exact model/quant (E11 probe).
- oLLM = thin Rust proxy, no llama.cpp submodule, backend via env vars.
- Scoring v0 = mtime LRU only, no density weighting yet.
- **The llama.cpp fork is DEAD (Rain, 2026-09-25).** `rain-sk/oLLM` remote deleted;
  local tree `~/source/llama-cpp-rain-sk` deleted along with its rescue bundle — the
  23 disk-cache commits are intentionally discarded ("we don't need the details").
  All durable value was already vendored into this repo at `docs/design/` (verified
  identical before deletion). C++ work restarts from `~/source/llama-cpp-upstream`
  @ `e70802a01` (build 10664 = exactly what llama-rocm2 runs today).

## Where everything lives

| what | path |
|---|---|
| proxy repo | box0 `~/source/oLLM` → `github.com/matarbot/oLLM` |
| scaffold mirror (pxl) | `/home/rain/ollm-scaffold/` (synced via scp) |
| backend key | box0 `~/ollm-cache/.backend_key` |
| start scripts | box0 `~/ollm-cache/{start-stack,start-backend}.sh` |
| tests | box0 `~/ollm-cache/{smoke,strict-write-test,stream-test,persistence-proof}.sh` |
| proxy log | box0 `~/ollm-cache/ollm.log` |
| cache blobs | box0 `~/ollm-cache/slots/*.bin` |
| E12 (test server slots + fixtures) | box0 `~/ollm-cache/e12/{slots,fixtures/<backend_sig>}` |
| E1 assets (reusable seed prompts) | box0 `~/ollm-cache/e1/` (e1.py, runlog.md) |
| C++ base (upstream clone @ running build) | box0 `~/source/llama-cpp-upstream` branch `running-build` = `e70802a01` |
| capacity ladder | pxl `~/projects/box0/sustained-agent-ladder.md` + `ladder.jsonl` |
| running design log | deep-memory `projects/ollm-proxy.md` |
| prior sessions | this one (`20260925_124705_14c21e`), ladder+enterprise session (`20260925_151716_8a011d`) |

## Verification ritual

After any change: `cargo build`, `cargo test` (7 protocol tests, no GPU),
restart via `start-stack.sh`, then
`bash ~/ollm-cache/strict-write-test.sh` — expect turn1 `remembered`,
a `blob published (atomic)` log line, turn2 recall `OMEGA-55`. If the
backend was restarted, confirm `backend_sig` first: if it changed, expect a
cold prefill on turn 1 (correct behavior, not a bug).
