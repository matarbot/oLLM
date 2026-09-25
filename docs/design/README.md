> **SUPERSEDED (2026-09-25):** the `--cache-disk` in-server tier described here was not implemented in this fork.
> The data model (§3 chained prefix hashes, §6 crash/race analysis, §7 compat gate) carries over to the Rust proxy
> **[matarbot/oLLM](https://github.com/matarbot/oLLM)**, which supplies the same policy from outside the server via
> `/slots/{id}?action=save|restore`. Active spec: oLLM README + docs/design/. This directory is retained as design history.

# Disk-backed prompt cache (`--cache-disk`)

Goal: `llama-server` keeps reusing a conversation's KV state **across restarts and
crashes**, so an agentic harness that thrashes the server does not pay a full re-prefill
every time. Sized by `--cache-disk <MiB>` (default `10240`) under
`--cache-disk-dir <dir>` (default `~/.cache/llama-server/`).

Status: **design reviewed and revised; not implemented.** Branch `disk-cache`, forked from
upstream `5ea87ddad`.

**v1 scope, stated plainly:** restore a conversation after a restart, crash or harness kill.
V1 does **not** share prefixes between concurrent sessions — two sessions with a common system
prompt each get their own entry, because a hybrid model's state cannot be truncated to a shorter
prefix (the recurrent state has already folded the tail in). Cross-session sharing is phase 2
(message-span-aligned publishing): `REVIEW-ANSWERS.md` §4.

**Read order:** [`REVIEW-QUESTIONS.md`](REVIEW-QUESTIONS.md) →
[`REVIEW-ANSWERS.md`](REVIEW-ANSWERS.md) → [`HANDOFF.md`](HANDOFF.md) → `design.md`. The two
REVIEW files carry corrections that supersede earlier text in this directory, including budget:
**300 GiB (`--cache-disk 307200` in the units), not 10 GiB.**

| doc | what's in it |
|---|---|
| [`llamacpp-cache-audit.md`](llamacpp-cache-audit.md) | how caching works today at `5ea87ddad`, with line numbers: `--cache-ram` prompt cache, context checkpoints, the `llama_state_seq_*` serialisation layer, `PARTIAL_ONLY` vs full states, why hybrid models (`qwen35`, `qwen4exp`) constrain the design, and the 10 gaps we are closing. |
| [`omlx-cache-analysis.md`](omlx-cache-analysis.md) | the behaviour contract extracted from [oMLX](https://github.com/jundot/omlx)'s tiered cache: parent-chained content hashes, publish-by-atomic-rename, read-your-writes over an async writer, compatibility signatures, index-derived-from-files, deferred cleanup of async stores. Adopt/adapt/drop list at the end. |
| [`design.md`](design.md) | the design: entry identity, file format, write/read paths, **the crash and race analysis (§6)** — two writers, torn files, extract-vs-cancel, restore-vs-decode, eviction-vs-read, disk-full — the compatibility gate (§7), CLI/API (§8), hybrid + MTP specifics (§9), alternatives rejected (§11), definition of done (§12). |
| [`REVIEW-QUESTIONS.md`](REVIEW-QUESTIONS.md) | independent review against the tree at `5ea87ddad` and the real GGUFs/host: 9 verified corrections (C1-C9), the missing L1-eviction publish trigger, the `--cache-ram 0` architectural question, 300 GiB consequences, durability specifics, 11 questions. |
| [`REVIEW-ANSWERS.md`](REVIEW-ANSWERS.md) | decisions on all 11: L2-as-L1-eviction-sink (W1/W2/W2p/W3 publish sources), slot-owned prefix identity with hooks outside L1, non-destructive shareable reads, v1 scope + phase reorder, no extraction on the critical path, per-unit cache directories, process-kill durability (header written once, mtime-only LRU), restore pre-validation contract, strict MTP policy, defaults, metrics. Ends with the doc edits still to apply. |
| [`container/README.md`](container/README.md) | recipe for a test image based on kyuz0's own runtime image (compile the fork, run it on port 1246, recover by deleting our tag). Recipe only — the build has not been run. |
| [`implementation-plan.md`](implementation-plan.md) | phased plan: Phase 0 build-environment blocker + three empirical experiments that must run before code, T1–T10 test matrix, benchmarks to record, risks, upstream review split. |

## Read this first if you only have five minutes

1. **There is no disk cache today.** `--cache-ram` is a RAM LRU that dies with the
   process; the only disk paths are the operator-driven `/slots/save` endpoints. A restart
   = full re-prefill.
2. **Our models are hybrid.** `qwen35`/`qwen4exp` mix full-attention layers with GDN
   recurrent layers. `llama_state_seq_*` with `FLAGS_NONE` captures *attention KV (grows
   with n) + recurrent state (≈153 MiB constant)*; `PARTIAL_ONLY` — what the existing
   checkpoints use — captures only the recurrent part and **cannot be restored into an
   empty context**. Any persistent snapshot must be `FLAGS_NONE`.
3. **The compatibility gate is the safety-critical part.** The on-disk KV layout changes
   with `--flash-attn` (because `attn_v_trans = !flash_attn`), `-ctk`/`-ctv`, `n_seq_max`,
   `n_ctx_seq`, `--kv-unified`, `--swa-full`, the chat template and the draft model. A
   mismatched restore yields *confidently wrong text*, not an error. Hence §7 and test T2.
4. **Crash tolerance comes from three rules**, all borrowed from oMLX: content-addressed
   keys (so an aborted or killed request can only waste work, never corrupt), publish by
   atomic rename + dir fsync (so nothing half-written is ever visible), and derive the
   index from the files at startup (so there is no index to lose).
5. **`flock` is ours, not oMLX's.** They assume one process. On this box an orphaned
   `podman exec`'d `llama-server` plus a systemd-started one is a real state, so the second
   writer becomes read-only instead of racing.
