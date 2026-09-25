# Phase 0 hand-off — build environment + the four experiments

For the agent picking up `implementation-plan.md` Phase 0. No cache code in this phase.
Everything below that is a *decision* (not a fact) is marked **(open)** and lives in
`REVIEW-FOLLOWUP-2.md` — do not re-litigate or "fix" it while doing Phase 0.

## 0. Where we are

* Design is settled and reviewed. Read order: `REVIEW-ANSWERS.md` -> `REVIEW-ANSWERS-2.md` ->
  `REVIEW-FOLLOWUP-2.md` -> `HANDOFF.md` -> `design.md` -> `implementation-plan.md`. The
  answers files supersede the design/plan wherever they conflict; the supersession banner at
  the top of the plan is the single source of that rule.
* Phase 0 = (a) build the test container (recipe in `container/README.md`, **not yet run**),
  (b) four experiments **T0.0 - T0.3**. Their results gate Phase 1 and three of them
  produce numbers the design still contains as estimates.
* Dev/acceptance target: **the 27B unit** (`qwen35`, `--parallel 3`, MTP draft). Flash-Next
  is stopped for the build phase. Fork tests run on port **1246** in container **llama-fork**;
  port 1245 and the two toolbox names are production and must not be touched.

## 1. Facts you can rely on (verified 2026-08-28)

| fact | value / note |
|---|---|
| RAM | 121 GiB unified, **~6.6 GiB available** at hand-off. The 27B unit, if running, pins `--cache-ram 32768`. **Any experiment that loads the 27B model needs the unit stopped first** (a second 16 GiB load + KV does not fit in 6.6 GiB). Coordinate the downtime window with the principal - hermes uses the server. |
| build hazard | a 32-job HIP build while a server is resident is a swap storm/OOM. `systemctl --user stop` the unit first, or pass `BUILD_JOBS=6`. |
| disk | `/home` is btrfs, 952 GiB, 725 GiB free. Fork test cache root: `~/.cache/llama-fork/`. Never commit `*.gguf`. |
| models | `~/models/qwen3.8-27b/Qwen3.8-27B-UD-Q4_K_XL.gguf` (~16.3 GiB) + `~/models/qwen3.8-27b/MTP/mtp-Qwen3.8-27B-Q4_0.gguf` (~1.3 GiB, arch `qwen35`, 18 tensors). |
| parity patches | **both** (`llama-grammar.patch`, `llama-cpp-25992-rocm-host-buffer.patch`) are absent from this fork and required for comparable numbers. The Dockerfile applies them with `APPLY_PATCHES=best` - read its stdout and record which actually applied. |
| setup repo | `~/source/setup` is **not editable from here** (its AGENTS.md says so). Benchmark rows land in `findings/` here and are handed to the principal. |

## 2. Order of work (and why it is not the plan's order)

**T0.0 and T0.3 need no fork build at all** - do them first, in the window before/while the
20-60 min HIP compile runs. T0.1 and T0.2 need the fork binary.

### Step 1 - T0.3: real long-prompt pp baseline (no build, ~1 h)

`setup/benchmark.md` has no usable prefill number (all recorded `pp` came from ~20-token
prompts). Measure cold prefill at **2k / 8k / 32k / 64k tokens** on the **production 27B
unit as it runs today** (that is the baseline the disk cache is judged against):

* method: `POST /v1/completions`, `temperature 0`, `max_tokens 96`, read `timings.prompt_per_second`;
  one request per length, **first request after a server start** (cold KV), two runs per
  length, keep the mean;
* prompt construction: deterministic, long, hermes-shaped (system prompt + tool definitions +
  a few turns). Record exactly how the prompt was built and its token count in the findings
  file - a pp number without the prompt is useless;
* record in `findings/03-pp-baseline.md`: per length, cold pp (t/s), wall time, and the
  derived cost per token. This fills `--cache-disk-pp-tts` (design §11 metrics) and the
  setup/benchmark.md rows to hand over.

### Step 2 - T0.0: hermes prefix stability (no build, ~1 h + observation)

The whole chain-hash identity presumes the harness's prompt is **append-stable until
compaction** (principal's experience; must be measured, not assumed - the plan banner's
"verified by T0.0" is a wording bug until this runs):

* capture two consecutive hermes turns' request bodies without patching the fork: a small
  logging TCP proxy in front of `127.0.0.1:1245` (or mitmproxy) that stores each
  `/v1/chat/completions` body;
* compare at the **message level**: is `messages` append-only (same array, one new user
  message + the previous assistant message prepended, nothing else changed)? Any per-turn
  mutation - a clock in the system prompt, injected context, reworded history - breaks the
  chain from turn 1 and the design changes (identity per request, not per prefix).
  Token-level comparison is a bonus, not required for the verdict;
* record: append-only yes/no, which fields (if any) mutate between turns, and *where
  compaction cuts* (likely unknown in one day - note "monitor", ask the principal).
* **If not append-stable: stop and escalate before any Phase 1 work.**
* `findings/00-prefix-stability.md`.

### Step 3 - build the test container (~1 h, unattended)

Follow `container/README.md` steps 1-3 exactly (ROCm block verbatim from upstream,
`APPLY_PATCHES=best`, install prefix `/opt/llama-fork`, tag `localhost/llama-fork:disk-cache-<sha>`,
run on 1246 with `--userns keep-id`). Before `podman build`:

1. `systemctl --user stop llama-server-27b` (and confirm 1245 is free: `ss -tlnp | grep 124`);
2. note `free -h` before and after (the build itself should not need much, the HIP link does);
3. read the build log's patch section - record which of the two parity patches applied.

Sanity gate (all four, in this order): `llama-server --version` reports our sha;
`ldd` shows the `/opt/llama-fork` libraries; the running `cmdline` is the 1246 one; a
20-token `temperature 0` completion on 1246 returns correct text. Record build time,
ccache behaviour (expected: host edits fast, HIP not cached), and the patch section in
`findings/00-build.md`. If the ROCm devel package cannot be installed, escalate - the
CPU-only fallback covers T1/T8/T9 later but **not** T0.1/T0.2/T0.3.

### Step 4 - T0.1: state round-trip equivalence (highest risk, ~2-3 h)

Prove that a `FLAGS_NONE` snapshot is a faithful, restorable copy of the per-seq state for
`qwen35`, in-process **and** across a process restart. If any part of the state is missing
from `state_write`, a restored snapshot is *silently wrong* and the design grows an extra
payload - this is the gate for everything.

Implementation: a small standalone C++ program (put it in `tests/` - the plan already adds
`tests/test-cache-disk.cpp`, and this is a private fork; name it `tests/test-state-roundtrip.cpp`,
built by the same container). Protocol:

1. load the 27B model **with the MTP draft context** (`--spec-type draft-mtp`, same flags as
   the unit), `temperature 0`, fixed seed; prompt of N tokens (start at 8192; add 1024 and
   32768 if the first passes cheaply);
2. **Run A (uninterrupted)**: decode the next 16 tokens, record ids;
3. **Run B (in-process round trip)**: fresh context, same prompt, then
   `llama_state_seq_get_data_ext(ctx_tgt, ..., FLAGS_NONE)` **and** `ctx_dft`, `seq_rm`,
   `set_data_ext` back, decode 16 tokens, record;
4. **Run C (cross-process)**: same as B but the blob is written to a file and the process
   exits; a new process loads model + file, restores into an empty context, decodes 16
   tokens;
5. compare A/B/C: token ids must be **byte-identical** at `temperature 0`. On mismatch,
   record the first divergence position and compare per-layer size breakdowns to see which
   part (attention vs recurrent vs draft) diverges - do not stop at "it failed".

Deliverable `findings/01-state-roundtrip.md`: verdict per run, any first-divergence data,
and the explicit answer to "is the MTP draft state fully in `ctx_dft`'s seq state?" (C9).
**If B or C fails: stop Phase 1, escalate with the divergence data.**

Fallback if the fork build is broken for longer than expected: T0.1/T0.2 only use the
`llama_state_seq_*` API, which the production toolboxes already ship - the same protocol can
run against the toolbox binary (server stopped) as an early signal, clearly labelled as such
in the findings file. It does not replace the fork run.

### Step 5 - T0.2: size vs extraction time (~1 h)

With the same tooling (or a flag on it), for n in {1k, 8k, 32k, 128k}:

* `llama_state_seq_get_size_ext` for `ctx_tgt` **and** `ctx_dft` separately (the draft blob
  grows with n too - C9 - and the design's "one blob dominates" assumption dies or lives on
  this number);
* `llama_state_seq_get_data_ext` wall time, i.e. the DtoH cost of a publish;
* repeat with `--flash-attn off` for one length (the value loop becomes ~16k backend calls,
  `src/llama-kv-cache.cpp:2183-2216` - we never run that mode, but the risk table and the
  W4 gate decision need the penalty number);
* derive the effective DtoH bandwidth (GiB/s). **This is the number for (open) Q-A:** the
  unified-memory estimate is single-digit to low-tens of ms per 16k publish; if T0.2
  confirms it, W4's gate should relax from `queue_tasks.empty()` to "no other slot
  decoding". Do not make that decision yourself - record the number and let the principal
  answer Q-A.

Validate (or correct) the arithmetic: `~153 MiB constant + ~34 KiB/token` for `ctx_tgt`.
Deliverable `findings/02-size-time.md`: the table, the bandwidth, the v_trans penalty, and
the formula-vs-measured deltas.

## 3. Definition of done for Phase 0

1. Container builds, A/B-able against production, parity patches' status recorded;
2. T0.1 passes in-process **and** cross-process (or the design changed and the principal
   signed off);
3. T0.2 numbers: size formula validated, DtoH bandwidth + v_trans penalty measured,
   `ctx_dft` sized separately;
4. T0.3: cold pp rows for 2k/8k/32k/64k with the prompt construction recorded;
5. T0.0: append-stability verdict (or escalation);
6. every experiment has `findings/NN-*.md` with date, config, exact commands, raw numbers,
   verdict, follow-ups;
7. production untouched: 1245, both units, both toolboxes, the kyuz0 images.

## 4. Escalate immediately (do not push through)

* **T0.1 mismatch** - any divergence between A and B/C. This is the whole project's
  highest-risk item and the only one that changes the design's payload format.
* **T0.0 not append-stable** - identity model changes.
* **ROCm devel package unavailable** - no HIP build, T0.1/T0.2/T0.3-on-fork all blocked.
* **OOM at any point** - a swap storm on this box corrupts nothing but wastes the session;
  stop, check `free -h`, re-plan the window.

## 5. Open items you must not touch while in Phase 0

From `REVIEW-FOLLOWUP-2.md` (answers pending from principal / planning agent): the W4/W1
role wording at plan `:187`, the "verified by T0.0" wording, W4's gate (Q-A - you only
*measure* the number), chain retention default (Q-B), the 27B unit's `--cache-ram 32768`
drop (a setup-repo change, and it is also a *prerequisite* for T0.1/T0.2 if the unit is
running), and the `--cache-disk-staging-mib` default.
