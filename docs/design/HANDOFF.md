# HANDOFF — context for whoever implements this

Read me first if you are an agent picking up the disk-cache work in this repo.

> **Order of reading:** `REVIEW-QUESTIONS.md` → `REVIEW-ANSWERS.md` → this file →
> `design.md`. A review of this handoff found real design bugs (extraction is a device transfer
> not a memcpy; hooks inside `server_prompt_cache` are dead when `--cache-ram 0`; L1 eviction is
> the missing publish trigger; the in-place header rewrite was the one corrupting-crash path).
> Those are resolved in `REVIEW-ANSWERS.md`, which **supersedes** this file wherever they
> conflict. Budget on the box is 300 GiB (`--cache-disk 307200` in the units), not 10 GiB. Everything
here was verified on the target machine (box0: Strix Halo 395, 121 GiB, Fedora 44,
rootless podman 5.8.4) on 2026-08-27 and is **not** in the other four documents.

## 0. Repo state at handoff

* branch `disk-cache`, based on upstream `5ea87ddad`, one commit: `faaba0ea2` (**docs
  only — no implementation exists yet**).
* `master` == `origin/master` == upstream. This fork has no local code commits.
* The machine-ops repo this fork sits next to is `/home/rain/source/setup` (private,
  `rain-sk/box0`). **Do not edit that repo from here.** It documents the host; it holds the
  systemd units, the benchmark table and the toolbox submodule.

## 1. Do not re-derive these (verified, with sources)

1. **Both models on the box are hybrid.** `Qwen3.8-27B` = arch `qwen35`,
   `Qwen3.8-Flash-Next` = arch `qwen4exp`; both carry `ssm.state_size=128`,
   `ssm.inner_size=6144`, `conv_kernel=4`, `full_attention_interval=4`. In this tree they
   resolve to `llama_memory_hybrid` / `llama_memory_hybrid_iswa`
   (`src/llama-model.cpp:2441-2480`).
2. **`PARTIAL_ONLY` is not restorable into an empty context.**
   `llama_memory_hybrid::state_write` (`src/llama-memory-hybrid.cpp:189-201`) skips the
   attention part under `LLAMA_STATE_SEQ_FLAGS_PARTIAL_ONLY`. llama.cpp's own context
   checkpoints use that flag (`tools/server/server-context.cpp:2247`), which is why they are
   useless across a restart. **A disk snapshot must be `FLAGS_NONE`.**
3. **Snapshot size on this box** ≈ 153 MiB constant (recurrent, f32, ≈49 layers) +
   ~34 KiB/token (16 attention layers × 4 KV heads × 256, `-ctk/-ctv q8_0`). At the 300 GiB
   budget the units will pass (`--cache-disk 307200`) that's ≈ 428 entries at 16k, ≈ 240 at
   32k, ≈ 68 at 128k; the flag default of 10 GiB is ≈ 14/8/2. **The per-conversation ceiling
   is `n_ctx_seq`, not `n_ctx`** (C8): the 27B unit runs `--ctx-size 786432 --parallel 3` ⇒
   `n_ctx_seq = 262144`, so one conversation's attention tops out ≈ 8.5 GiB regardless of the
   flag — the "zero entries at 786k" in the first draft conflated the two. The draft (MTP)
   blob grows with `n` too (C9) and is measured separately in T0.2.
4. **A mismatched restore is silently wrong, not fatal.** The attention buffer layout is
   built with `attn_v_trans = !cparams.flash_attn` (`src/llama-model.cpp:2459`) — i.e.
   **`--flash-attn` changes the on-disk V layout**. Also `-ctk/-ctv`, `n_seq_max`,
   `n_rs_seq`, `n_ctx_seq`, `--kv-unified`, `--swa-full`, the chat template, and the
   presence/identity of the draft context. Hence the compat gate (`design.md` §7).
5. **`GGML_ABORT` in the checkpoint path.** `common_prompt_checkpoint::load_tgt/load_dft`
   abort the process on a size mismatch (`common/common.cpp:2332`, `:2350`). Never route
   bytes that came from disk through those functions; they are RAM-only by design.
6. **The prompt cache consumes its entries.** `server_prompt_cache::load` erases the entry
   it restores (`tools/server/server-task.cpp:1856-1865`) — no refcount, no sharing. Don't
   assume an L1 entry survives a hit.
7. **`state_seq_get_data_ext` failure modes are silent**: exceptions are caught and turned
   into `return 0` (`src/llama-context.cpp:2987-2996`). Check the size contract yourself.
8. **Flash-Next is not in this fork.** `qwen4exp` support lives in
   `danielhanchen/llama.cpp` branch `qwen4exp/qwen3.8-flash-next` (upstream PR **#27793**;
   our machine docs previously said #27742 — corrected). You can implement and test the
   whole cache on `qwen35` (the 27B) without it; Flash-Next testing needs a branch that
   merges that source.

## 2. Build environment — settled, use the container

No dev toolbox is needed (that was the earlier plan; superseded). The toolbox project
ships the image recipes, and `amd-strix-halo-toolboxes/docs/building.md` documents local
builds with `--build-arg REPO/BRANCH`. Our recipe is in
[`container/README.md`](container/README.md) (Dockerfile text + build/run/recovery commands;
**the build itself has not been run yet**).

Verified facts about building here:

| fact | evidence |
|---|---|
| host has **no** `gcc`/`g++`/`cmake`/`ninja` (only `git`) | `command -v` on the host |
| runtime toolboxes cannot build: `hipcc` + `librocblas` present, but no `rocblas.h`, no cmake, no git | `podman exec llama-flash-next command -v ...` |
| **dev meta-package exists**: `amdrocm-core-devel7.14-gfx1151` from `https://repo.amd.com/rocm/packages-multi-arch/rhel10/x86_64` | `toolboxes/Dockerfile.rocm-7.14` (used by upstream CI 2 days ago) |
| podman 5.8.4 supports `--mount=type=cache` (use it for ccache) | built it |
| podman 5.8.4 supports named build contexts (`--build-context name=dir` + `COPY --from=name`) | built it |
| `RUN --mount=type=bind,source=/abs/host/path` **does not work** (source is context-relative) | build error: `resolving mountpoints ... sanitizing bind subdirectory` |
| `RUN --mount=type=bind,from=name,source=...` **fails with `Permission denied`** (SELinux on host home) | build error |
| ⇒ **use `COPY` for the source tree**, `--ignorefile` to exclude `.git` (377 MiB) | verified |

### ⚠ Two things that will bite you

* **Do not start a HIP build while a server holds the GPU.** Flash-Next IQ4_XS keeps
  ~110 GiB of the 121 GiB resident; `cmake --build -- -j$(nproc)` (32 hipcc jobs) on top of
  that is a swap storm / OOM. `systemctl --user stop llama-server` first, or cap to
  `-j 6`. This is unified memory, not a separate GPU budget.
* **Parity patches are mandatory and this fork has neither.** The production images apply
  `llama-grammar.patch` (`MAX_REPETITION_THRESHOLD` 2000 → 100000, needed for complex tool
  schemas — i.e. for hermes) and `llama-cpp-25992-rocm-host-buffer.patch` (disables pinned
  host buffers on *integrated* GPUs). Verified: `src/llama-grammar.cpp:13` here is still
  `2000`, and the #25992 symbols are absent from `ggml/src/ggml-cuda/ggml-cuda.cu`. Build
  without them and your benchmark numbers are not comparable to `benchmark.md` — and the
  #25992 patch is directly relevant to us, since the disk cache moves large host buffers on
  an integrated-GPU box.

## 3. Host runtime layout (so you don't break production)

* Production is **two systemd user units** in `~/source/setup` (`llama-server.service` =
  Flash-Next default, `llama-server-27b.service`), mutually exclusive, both bound to
  **port 1245** with alias `qwen3.8`, both `podman exec`-ing `/usr/local/bin/llama-server`
  *inside* the long-lived toolbox containers `llama-flash-next` / `llama-rocm-7.14`.
* **Use port 1246 for anything you build.** One `llama-server` per 1245; `ss -tlnp | grep
  1245` before starting. Never name a test container `llama-flash-next` or
  `llama-rocm-7.14` — the units and `refresh-toolboxes.sh` key on those names (and a
  refresh **deletes and recreates** them).
* Toolbox container flags to mirror (exact bytes from `refresh-toolboxes.sh`):
  `--device /dev/dri --device /dev/kfd --group-add video --group-add render
  --group-add sudo --security-opt seccomp=unconfined`, plus `--network host`,
  `--userns keep-id`, `-v /home/rain:/home/rain`.
* `/home/rain` **is** bind-mounted into the toolboxes, so a cache dir under
  `~/.cache/llama-server/` is host NVMe from inside the container. A plain
  `podman run` must pass the mount explicitly, and without `--userns keep-id` it writes
  root-owned files into the user's home.
* Models: `~/models/qwen3.8-27b/Qwen3.8-27B-UD-Q4_K_XL.gguf` (+ `MTP/` draft),
  `~/models/qwen3.8-flash-next/{UD-IQ4_XS,UD-Q2_K_XL}/`. Never commit a `*.gguf`.
* Disk: 725 GiB free on the same btrfs volume as the weights. Budget: 300 GiB
  (`--cache-disk 307200`) — validate against `statvfs` free space at startup and on `ENOSPC`,
  count in-flight `*.tmp` bytes in the quota (CoW ⇒ no shared extents), `posix_fadvise`
  `DONTNEED` after every read/write so the cache does not evict the weights' page cache.

## 4. Invariants to preserve while implementing (short version)

1. Only the server thread touches `llama_context`; the writer thread gets owned
   `std::vector<uint8_t>` and does plain file I/O only.
2. Identity = hash chain over the *token prefix*, rooted in a compat key. Never key by
   slot id — that is what makes aborted/killed requests harmless.
3. Publish = write tmp → `fsync(file)` → `rename` → `fsync(dir)`. Nothing may ever be
   readable half-written.
4. The index is **derived from the files** at startup (header-only scan). No index file can
   be lost. Converge the quota at startup before serving.
5. One `flock` on `<dir>/.lock` for the process lifetime; `EWOULDBLOCK` ⇒ read-only mode
   (loads yes, stores/evictions never). Released by the kernel on `SIGKILL` — that is the
   thrash case we are designing for.
6. All index mutation + eviction + unlink on the single maintenance thread; readers
   tolerate `ENOENT`; deferred unlink with a grace period; pin in-flight prefetches.
7. Publish sources are W1 (L1 eviction — free, bytes already host-side, the primary path),
   W2 (idle, queue empty), W3 (shutdown drain); **extraction never runs on a request's critical
   path**, and post-prefill extraction (W2p) is off by default. Dedup by key. Extraction is a
   synchronous **device transfer** — `ggml_backend_tensor_get` per (tensor, range) in the io
   destructor, ~32 calls in `!v_trans` mode vs ~16k in `v_trans` mode — not a memcpy; see
   `REVIEW-QUESTIONS.md` C1/C2 and `REVIEW-ANSWERS.md` §5. Get the T0.2 number before tuning.
8. Every disk failure ⇒ cache miss + counter. No new `GGML_ABORT`, no new crash path, and
   `--cache-disk 0` must produce byte-identical behaviour to today's `master` (no threads,
   no directory created, no log noise).
9. Counters distinguish *queued* from *durable*. Report `saved_prompt_tokens` so the win is
   measurable, and add rows to `~/source/setup/benchmark.md` (that file is the box's source
   of truth for performance claims; it currently has **no usable prefill baseline**).

## 5. Order of work (do not skip step 1)

1. **T0.1 state round-trip equivalence** — see
   [`implementation-plan.md`](implementation-plan.md) §0.2. Prove that
   `FLAGS_NONE` → `seq_rm` → `set_data_ext` → next-token is identical to the uninterrupted
   run for `qwen35`, in-process *and* after a process restart. If this fails, the design
   changes and everything else waits.
2. Format + compat key + store primitives (pure, unit-testable, no llama.cpp state).
3. `flock` gate, writer thread, index/lookup.
4. Server glue: restore side, then store side. Correctness gate: byte-identical
   `temperature 0` continuations cold vs restored at ≥ 3 prompt lengths.
5. Prefetch/staging. Metrics, flags, docs.
6. Chaos suite (T4–T10 in the plan) before believing any of it.

## 6. Open questions nobody has answered yet

* Does `qwen4exp`'s indexer / PLE state (`qwen4exp.attention.indexer.*`,
  `qwen4exp.ple.conv_kernel`) appear in `llama_memory_*::state_write`? If not, restored
  Flash-Next snapshots are silently wrong and the payload needs an explicit extra section.
  Same class of question for the MTP draft context (different memory type —
  `mtp_on_hybrid_qwen`, `src/llama-model.cpp:2412-2416`).
* Should the on-disk recurrent state be down-cast (f32 → bf16) to double entry density?
  oMLX quantises GDN state further (int8/RHT codecs). Correctness risk; phase 4.
* What is the actual pp rate at 8k/32k tokens (T0.3)? Without it we cannot state the win,
  and the whole project's payoff estimate is currently a guess.
