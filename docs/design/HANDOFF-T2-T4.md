# HANDOFF — T2 (restore correctness) + T4 (chaos / durability)

Status at hand-off: Slice A (W3 restore, W4
finish/cold publish, W5 shutdown) and Slice B (W1 evict sink, W2 idle trigger,
Q5 consume fix) are implemented, committed, and live-verified. This hand-off
scopes the next work: test-plan items **T2** and **T4** from
`implementation-plan.md`. No T2/T4 work has started.

Scratch file — delete when T2/T4 are recorded in the plan doc.

---

## 1. Repo / branch state

* Repo: `/home/rain/source/oLLM` (llama.cpp fork), branch **`disk-cache`**
  (an earlier hand-off called it `disk-cache-poc` — wrong, no such branch).
* HEAD: `a43f9c40e`, working tree clean.
* Disk-cache commits (newest first):
  * `a43f9c40e` docs: record slice B decisions and live test
  * `7a6bbf163` test: cover the L1-evict sink handoff
  * `c51d7b68e` server: publish evicted and idle conversations to the disk cache
  * `55a5fbdb1` docs: note plan to verify behavior on Windows and macOS
  * `8617648cd` docs: align disk-cache docs with the implemented flags and slice A
  * `2a7032ed3` server: publish finished conversations to the disk cache and restore them on admission
  * `1e6552231` test: fix disk-cache test suite for Release builds
  * `e675e051b` server: build the disk-cache store on non-POSIX platforms
  * `dfcfbf265` server: let disk-cache store open accept a trailing slash and missing ancestors
  * `a444c7eaf` server: store the tgt/dft payload split and full header crc in disk-cache entries
  * `4b8264ee3` server: add disk-cache index, chain hash and F3 dedup
  * `a3500d5cf` server: add disk-cache writer thread with bounded queue
  * (earlier: `b1586d979` flock store gate, `218443d33` T0.1 round-trip, format/compat-key/store)
* Commit style: `server:` / `test:` / `docs:` prefix, lowercase concise subject,
  short body paragraphs, trailer `Assisted-by: pi (llama.cpp qwen3.8-27b)`.
  No `Co-authored-by`. **No push, no PRs, no commit without explicit
  per-commit approval from the principal.**

## 2. What is already proven (do not redo)

* **T0.1 state round-trip: PASS on the real 27B hybrid** —
  `findings/01-state-roundtrip.md` (2026-08-29). `Qwen3.8-27B-UD-Q4_K_XL` +
  MTP draft (arch `qwen35`), 8192-token prompt: `state_seq_get/set_data_ext`
  of **both** target and draft contexts is bit-identical in-process and
  cross-process. The top design risk (payload omits hybrid state) is closed.
* **Live 8B tests (Slice A + B, 4 runs):** publish paths all fire correctly —
  W4 cold/finish (busy-gated when a peer decodes), W1 evict (dedup when the
  prefix is already on disk; visible 4149 tok / 310 MiB write otherwise),
  W2 idle (published a released slot, `task -1`), W3 restore (a W1-written
  entry restored in 12.6 ms; request 1.14 s vs ~22 s cold). Unit suite:
  72 tests / 325 assertions / 0 failures, including store crash unit tests
  (`fork` + `_exit` mid-write) and the W1 sink contract tests.
* **The T2/T4 gap:** (a) no *byte-identical continuation* comparison through
  the real server path (publish -> restart -> restore -> continue); (b) no
  live `kill -9` chaos against a running server.

## 3. T2 — restore correctness (the silent-wrongness gate)

Plan definition: *golden `temperature 0` continuations, cold vs restored, at
several prompt lengths, for `qwen35`.*

What exists is "restore is fast and produces output". What is missing is
"restore produces **identical** output to a run that never left the live
context". T0.1 proved the blob round-trip at library level on `qwen35`; T2
proves the *server* path (admission -> L1/L2 lookup -> restore -> prefill of
the tail -> decode) end to end.

### Harness (scripts in `/home/rain/t01/`, outside the repo — no new files in
`tests/` without maintainer approval; record results in the plan doc)

Copy the pattern from `/home/rain/t01/server7/run_slice_b_test.sh` +
`slice_b_test.py` (server lifecycle, health loop, phase scripts).

Determinism: `--temp 0`, one conversation per request, **identical flags on
both processes** (the compat key enforces this anyway — a config drift means
no restore, which the log shows, so a mismatch fails loudly, not silently).

For each prompt length N in {1024, 4096, 12288} (8B runs `--ctx-size 16384`;
keep `--parallel 1` for clean slot attribution), fixed prompt corpus text,
`max_tokens` = 64:

1. **run A (cold):** fresh cache dir, process 1. Request P. Record `content`
   + `tokens` count + the prompt-eval time from the log. (W4 publishes at
   finish; send SIGTERM so the writer drains.)
2. **run B (restored):** process 2, same cache dir, same flags. Request P
   again. The log must show `disk cache: restored N+64 tokens ... in X ms`
   (or the L1-tier equivalent if L1 still holds it — for a *clean* L2 test,
   use `--cache-ram 0` on run B so any hit is L2 by construction; that is the
   T6b insight applied to T2). Record `content` + `tokens`.
3. **Assert:** run A `content` == run B `content` byte-for-byte, token counts
   equal, run B prompt-eval time ~ 0 for the restored prefix.
4. Optional control: same process, second request after a slot change (exercises
   the L1-consume path instead of L2) — must also match.

Compare `content` text (plus token count), not raw ids — greedy decoding from
identical state is deterministic, and text is what a user would see differ.
If a mismatch ever appears, the first suspect is a *non-restored* state
component or a prefill-tail boundary bug in `launch_slot_with_task`, not the
writer.

**8B vs 27B:** the plan's T2 says `qwen35` (the 27B unit). T0.1 already closed
the payload question on `qwen35`, so an 8B T2 (cheap, no coordination) covers
the server glue; a 27B T2 additionally covers the hybrid glue end to end but
needs the running 27B unit (port 1245) stopped for the window — **principal's
go-ahead required** (see §6 incident note). Do the 8B version first, record
both in the plan doc, and let the principal decide about the 27B window.

## 4. T4 — chaos (durability promise = process-kill)

Plan definition: `kill -9` at mid-extract, mid-write (before rename), after
rename before index, mid-restore, mid-evict, during shutdown drain -> assert
(a) no crash, (b) cache either unchanged or consistent, (c) T2 still passes,
(d) quota respected, (e) at most the in-flight publishes are lost. Power loss
is out of scope (label it as such in the report).

Store guarantees already in the code (so the test asserts, not rediscovers):
atomic publish tmp -> fsync -> rename -> dir fsync; the index is rebuilt by
header-only scan at startup (there is no separate index file — "after rename,
before index" simply means the entry is found on the next scan); `sweep()` at
startup removes orphan `*.tmp` (`server-context.cpp:1017`, rw mode only);
flock keeps a second process `ro`.

### Harness sketch

* Small budget (e.g. `--cache-disk 256`) with 12k-token prompts (~300 MiB
  entries) so every publish forces LRU eviction — makes mid-evict kills
  routine instead of rare.
* Workload: a loop of sequential long-prompt requests (continuous
  extract -> queue -> write -> evict cycles).
* Killer: background loop sending `podman kill -s KILL <name>` at random
  0.1-1 s intervals for a fixed duration (e.g. 3-5 min); restart the server
  after each kill, assert the startup invariants, continue the workload. A
  few hundred kills across the windows is plenty — the windows are small but
  the cycles are continuous, so all five points get hit repeatedly.
* Dedicated windows worth forcing explicitly:
  * **mid-extract / mid-write:** kill immediately after the request returns
    (the publish extraction runs at finish; a 300 MiB extract+write spans
    many ms).
  * **mid-restore:** kill within ~100 ms of a request that the log shows
    restoring (12k restore takes well over the kill-loop granularity).
  * **shutdown drain:** SIGTERM, then `kill -9` inside `--cache-disk-drain-ms`.
* Per-restart assertions:
  a) server reaches `{"status":"ok"}` and logs the `disk cache: mode=...` line;
  b) **orphan cleanup is observable** — OPEN ITEM: `sweep()` returns a count
     that is not logged; add one `SRV_INF("disk cache: swept %zu orphan tmp
     files\n", n)` (small code change, needs its own commit);
  c) index consistent: `entries=K` in the startup log == `find <dir> -name
     '*.lkv' | wc -l` after sweep; a second clean restart reports the same K;
  d) quota: `used` <= budget (+ at most one in-flight entry);
  e) T2 spot-check: pick one surviving entry, run the §3 cold-vs-restored
     comparison against it — continuation must be byte-identical;
  f) loss bound: the number of `published` log lines across the run >=
     entries on disk + dedups + drops (nothing more than in-flight work is
     lost; dedups are visible as `put()` false + `known()` in the log).

## 5. Build / test environment (the error-prone facts)

* **Builder:** persistent container `llama-fork-builder-ct` (up 7+ days).
  The source tree is an **in-container copy** at `/opt/llama-fork-src` — the
  host `/opt` is empty and nothing under it is a mount. Sync from the host
  tree *inside* the container (it mounts `/home/rain`):
  ```
  podman exec llama-fork-builder-ct bash -c \
    'cp /home/rain/source/oLLM/<path> /opt/llama-fork-src/<path> && cd /opt/llama-fork-build \
     && cmake --build . --target llama-server test-cache-disk -j 14'
  ```
  (rsync does not exist in the container; plain `cp` per file is fine.)
* **Staging (both artifacts!):** the `llama-server` binary is a ~12 KB
  launcher that dlopens `libllama-server-impl.so` through
  `LD_LIBRARY_PATH=/home/rain/t01/server7/lib`. Copy **both** after a build:
  ```
  podman exec llama-fork-builder-ct bash -c 'cp -f /opt/llama-fork-build/bin/llama-server /home/rain/t01/server7/llama-server \
    && cp -f /opt/llama-fork-build/bin/libllama-server-impl.so /home/rain/t01/server7/lib/'
  ```
  Copying only the launcher silently runs the previous library. Verify with a
  sentinel string instead of trusting timestamps:
  `grep -c '<sentinel>' /home/rain/t01/server7/lib/libllama-server-impl.so`.
* **Clocks disagree:** the container clock is hours behind the host. Never
  judge build freshness by mtime — use sentinel strings.
* **Test server:** image `localhost/llama-fork:disk-cache-8a1a571ae`,
  container name `t2-cache-test`, port **1246** (1245 is the 27B unit —
  never touch), model `/home/rain/models/qwen3-8b/Qwen3-8B-Q4_K_M.gguf`,
  `--device /dev/dri --device /dev/kfd --group-add video --group-add render
  --security-opt seccomp=unconfined --security-opt label=disable
  -v /home/rain:/home/rain`. Reference run: `run_slice_b_test.sh` (run 4
  block). The port is reachable **only via `podman exec <name> ...`**
  (pasta networking); health loop: wait for `{"status":"ok"}` — a 503
  "Loading model" is NOT healthy; cap the loop at 120 s.
* **Unit suite:** `podman exec llama-fork-builder-ct bash -c 'cd /opt/llama-fork-build && ./bin/test-cache-disk'`
  (expect 72 tests / 325 assertions / 0 failures before this work starts).
* **Load discipline:** the 27B unit shares the box's unified memory. One
  8B test server (2 slots, ~5-6 GiB) is fine; do not stack test servers and
  do not run long concurrent model loads — a previous test-server burst put
  the 27B unit into a waiting state that the principal had to reset.

## 6. 27B unit — do not touch

`llama-rocm-7.14` serves the 27B model on port 1245. It is production.
Incident on record: a test-server workload burst put it into a waiting state
(principal reset it). Any T2-on-27B needs an explicit stop window approved by
the principal; everything else runs against the 8B test server on 1246.

## 7. Open details to settle when work resumes

1. `sweep()` count is unlogged — add the one-line `SRV_INF` (T4 assertion b
   depends on it); tiny standalone commit.
2. T2 output comparison: `content` text + token count (proposed) vs raw token
   ids — confirm with the principal (ids need a server response field that
   may not exist on this endpoint; text needs nothing).
3. T2 lengths: {1024, 4096, 12288} on 8B (`--ctx-size 16384`); for the 27B
   version the plan's `qwen35` unit has `n_ctx_seq = 262144` — pick 3 lengths
   there too if the principal opens a window.
4. T2 run B uses `--cache-ram 0` for a clean L2-only measurement (T6b
   insight); keep one run with L1 on to cover the L1-consume path (slice B
   Q5 code) as well.
5. Results go into `implementation-plan.md` as a "T2 / T4 results" note next
   to the slice notes (same style), plus a plan-doc tick of the test table.
   Harness scripts stay in `/home/rain/t01/` (outside the repo).
6. After T4: consider whether the `ENOSPC` path (T7) is cheap to piggyback on
   the same harness (loop-backed 200 MiB dir) — the principal's call.

## 8. Definition of done for this hand-off

* T2: for each length, cold == restored byte-identical, restore visible in
  the log, prompt-eval ~ 0 on the restored prefix; results recorded in the
  plan doc; 27B run done or explicitly deferred with the principal's sign-off.
* T4: the kill loop ran across all five windows + shutdown drain; every
  restart passed assertions a-f; at most in-flight publishes lost; results
  recorded in the plan doc (label the durability promise as process-kill,
  power loss out of scope).
* Unit suite still green; no regressions in the 4-run slice test; working
  tree committed in `server:`/`test:`/`docs:` increments with the
  `Assisted-by` trailer; nothing pushed.
