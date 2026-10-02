# HANDOFF — oLLM block-level prefix sharing (next step)

Snapshot: 2026-09-30 ~11:20 CEST. All live-state claims below were probe-verified
at this timestamp. Read top to bottom once; re-read Pitfalls before touching code.

## Live state (verified 2026-09-30)

- **box0** (Strix Halo, 192.168.178.42, SSH from pxl open): prod stack runs inside
  podman container `ollm-backend` (renamed from llama-rocm2 2026-10-02; image kyuz0/amd-strix-halo-toolboxes:rocm-7.14).
  - backend `:1245` = **our fork's patched HIP build**:
    `~/source/oLLM/vendor/llama.cpp/build-hip/bin/llama-server`
    (branch `ckpt-persist`, tip `64dfa8058`, gfx1151 — vendored as submodule `vendor/llama.cpp`; rebuild via `bash ops/build-vendor-hip.sh`). Flags: `--parallel 3
    --ctx-size 786432 --cache-ram 32768 --slot-save-path
    /home/rain/ollm-cache/slots` + MTP draft.
  - proxy `:1247` = `~/source/oLLM` `target/debug/ollm`, bound 0.0.0.0,
    `backend_sig=a67e59194299` (must match blob filename suffix).
  - health: both `status ok`. NO production consumers — this is a live dev/test
    stack; restarts are hygiene, not risk events.
  - cache dir: 2.9 GB / 6 blobs / limit 2048 MB ⇒ **currently over-limit,
    LRU churns every save**. First knob to fix as part of the next step.
- oLLM repo `matarbot/oLLM` main @ `f32d013`, clean. Design corpus +
  `docs/design/checkpoint-persist.md` (root cause, fix, measurements) all pushed.
- llama.cpp fork `matarbot/llama.cpp` branch `ckpt-persist` pushed (tests-first:
  failing tests -> save sidecar -> restore rebuild). Upstream issue #25913 is the
  bug this branch fixes.

## Done-and-verified (do NOT redo)

- E12/E12h silicon verification: restore byte-exact, checkpoint sidecar `.ckpt`
  (magic LCKP) written+restored, warm-after-restart prompt reuse 22k tokens
  (prompt_n=17, 0.32 s vs cold 63 s). Evidence: `~/ollm-cache/e12/` +
  HANDOFF commits `0b62fac` era.
- llama.cpp CPU CI-equivalent suite on fork: 391 passed post-rebase (py3.11 venv
  `~/venv-server-test-311`, `tools/server/tests/tests.sh`, LLAMA_SERVER_BIN_PATH).
- oLLM proxy fixes, each red-test-first, green, deployed (`matarbot/oLLM`):
  1. sidecar rides blob renames + eviction (`bd851de`->`2049f3c`)
  2. header-less sessions stick per-conversation, system-prompt-only hash
     (2 commits; 13/13 `cargo test`)
  3. client disconnect ABORTS backend generation (`6b7a356`->`3db5aa5`); live
     check: hang up at 1.1 s -> backend idle at 1.6 s.
- cache-tier A/B measured (`f32d013`, design doc): `--cache-ram 0` kills ALL
  reuse incl. hot-slot self-reuse; two-tier config is canonical. Do not
  relitigate without new hardware/config facts.

## Open work — the next step: block-level prefix sharing (fork-era design, v0 gap)

Goal: blobs share common prefix SEGMENTS on disk instead of each session blob
holding a whole-slot copy. Fixes: (a) 800 MB of duplicated base observed with 3
parallel similar sessions today, (b) makes the disk tier do real per-prefix work
so RAM tier becomes a speedup, not the only mid-conversation cache.

Design inputs (all in `matarbot/oLLM/docs/design/`): `design.md` (chain-hashed
block keys, compat-key root), `llamacpp-cache-audit.md`, `implementation-plan.md`.
Constraint from llama.cpp today: slot save/restore is whole-slot only
(FLAGS_NONE blob, ~153 MB floor + ~34 KB/token). Block sharing therefore needs
either a fork-side partial save/restore (build on ckpt-persist sidecar plumbing —
same team owns both) or proxy-side blob surgery (concat segments, write merged
blob before restore; llama.cpp must then trust it — see checkpoints guard,
server-context.cpp `forcing full prompt re-processing`).

Suggested first milestone: measure the fork's partial-state path
(`load_state PARTIAL_ONLY`, used by ckpt restore) for feasibility of
"assemble KV from shared blocks + restore". RED test first against the existing
`tools/server/tests/unit/test_slot_save_checkpoint_reuse.py` harness pattern
(tinygemma3, cache_ram=0). Decide fork-vs-proxy design BEFORE coding; large
changes upstream need an issue + maintainer discussion first (their AGENTS.md),
but matarbot private fork is exempt from that gate — still write the design doc
in `docs/design/` as a PR-style proposal Rain can open.

## Pitfalls (each cost real time this month)

- **Never reuse old blob names without the sig suffix check**: blobs are keyed
  `<session>__<backend_sig>.bin`; sig change (model/template/slots change)
  means restore silently re-prefills. That is correct, not a bug.
- **`/v1/chat/completions` ignores query-string `id_slot`**; pin via native
  `/completions` body, and keep seed/warm `reasoning_effort` identical —
  it injects template vars, forked prompts at token 9 once and cost a full
  false-negative silicon diagnosis.
- **`--cache-ram 0` looks philosophically pure, is latency poison** — see A/B.
- **Overwrites are last-publish-wins**: a concurrent same-session steer publishes
  over an in-flight turn's blob; tail of one branch clobbers the other's.
  Correct-by-rules, but attribute TTFT/recall anomalies accordingly.
- **Parallel identical prefixes cannot share on first round** (RAM cache serves
  settled prefixes, not in-flight). Thundering herd = physics.
- **Sidecars**: every blob op moves/deletes `<blob>.ckpt` alongside. Proxy has
  helpers (`sidecar_path`, `rename_sidecar` in lib.rs) — use them, don't
  hand-roll renames.
- **Tests never create repo folders**; suite convention is `./tmp` exists
  (CI provides). No Qwen3.8-27B downloads in tests; tinygemma3 is the suite's.
- **box0 test servers on `:1246`**: kill by pattern `llama-server` inside
  container (`podman exec ollm-backend pkill -f llama-server`) — it also kills
  prod `:1245`; that is fine ONLY because there are no prod consumers; announce
  it, don't let Rain's live session silently die mid-thrash.
- **abort is per-connection**: llama.cpp cancels on HTTP disconnect; the proxy's
  pump now closes the backend stream on client hang-up. If you reintroduce
  "drain anyway to persist", you re-create the steer-storm (zombie generations).

## Settled decisions (+ revisit triggers)

- Two-tier cache canonical (revisit: if RAM pressure forces it, and only with
  fresh A/B numbers).
- Disk forest stays whole-slot until block-sharing lands (revisit: after
  PARTIAL_ONLY feasibility test above).
- PR text authored by Rain only — upstream AGENTS.md forbids AI-written PRs /
  commit messages / comments; commits here carry `Assisted-by` style only when
  Rain writes them. NEVER `gh pr create` upstream.
- oLLM has no prod consumers (revisit: when MyAgent ships against it, rerun
  the thrash suite as a release gate and revisit cache_limit + weak LAN auth).

## Loose ends that need Rain

- Raise `cache_limit_mb` (2048 is being churned) or clean stale blobs — needs
  Rain's disk budget call.
- Proxy has no inbound auth (LAN-only, key posture inherited); decide before
  anything production touches it.
- Upstream PR for ckpt-persist branch still Rain's to open (template exists).

## Verification ritual (run after any change)

```
ssh rain@192.168.178.42
curl -s 127.0.0.1:1247/health                      # expect status ok, sig a67e59194299
cd ~/source/oLLM && cargo test                      # expect 13 passed 0 failed
bash tests/live/strict-write-test.sh              # expect recall OMEGA-55, blob+ckpt pair
python3 tests/live/parallel_similar.py            # expect 3x cold TTFT ~10-35s
python3 tests/live/abort_live.py                  # expect backend idle <2s after hangup
```
Backend restart (~3 min): `bash ops/start-backend.sh` (idempotent; health-polls itself).
Full teardown: `bash ops/stop-stack.sh`. Front door up: `bash ops/start-stack.sh`.
Scary-but-correct: sig changes on restart ⇒ every blob name mismatches ⇒ turn-1
cold prefills everywhere. Delete stale blobs to clear, never "fix" by pinning sig.

## Path table

| what | where |
|---|---|
| oLLM repo | box0 `~/source/oLLM` (remote matarbot/oLLM) |
| llama.cpp fork work | box0 `~/source/oLLM/vendor/llama.cpp`, branch `ckpt-persist`; pxl read-clone `~/source/llama-cpp-work` |
| launch/build scripts | repo `ops/` (start-stack, start-backend, stop-stack, build-vendor-hip; shared knobs in ops/common.sh) |
| live test harnesses | repo `tests/live/` (strict-write, smoke, persistence-proof, stream, abort_live, parallel_similar) |
| runtime state (untracked) | box0 `~/ollm-cache/` (`.backend_key`, `slots/`, logs) + archived `e12/`, `e1/` |
| design corpus | `docs/design/` in oLLM repo |
| deep-memory | `projects/ollm-proxy.md`, `projects/ollm-fork.md` |
