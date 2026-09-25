# T0.1 - state round trip gate

Date: 2026-08-29. Binary: fork `test-state-roundtrip` (compiled inside the
fork builder container, linked against `/opt/llama-fork/lib64`), run in the
fork runtime container with GPU. Model: `Qwen3.8-27B-UD-Q4_K_XL` +
`mtp-Qwen3.8-27B-Q4_0` (arch `qwen35`, `--spec-type draft-mtp
--spec-draft-n-max 3`). Flags mirror the production unit:
`--ctx-size 16384 --parallel 1 --n-gpu-layers -1 --cache-type-k q8_0
--cache-type-v q8_0 --flash-attn on --load-mode none --batch-size 2048
--temp 0 --n-predict 16 --seed 0`.

Gate question: does `llama_state_seq_get_data_ext` / `set_data_ext` round
trip the **whole sequence state of both the target context and the MTP
draft context**, in-process and across processes, such that decoding after
restore is bit-identical to decoding that never left the live context?
This is the payload definition of an L2 snapshot.

## Method

Deterministic harness (`tests/test-state-roundtrip.cpp`): an 8192-token
prompt built from repeated technical-prose lines (tokenised, exact count
enforced), greedy decoding (`--temp 0 --seed 0`), 16 output tokens printed
as raw ids. Five modes, one process each, runner in `t01-run.sh`:

| mode | what it does |
|---|---|
| A | prefill 8192 tokens (chunked at `n_batch` like the server), decode 16 |
| A2 | identical to A, second process - harness determinism check |
| B | A, then wipe both seqs in place, restore both blobs from memory, decode 16 |
| C1 | A, then `get_data_ext` both seqs to disk (`state_tgt.bin` / `state_dft.bin`) |
| C2 | fresh process, fresh contexts, `set_data_ext` both blobs, decode 16 (no prefill) |

Compare the 16 ids across A / A2 / B / C2.

## Result

**PASS.** All four id streams identical:

```
A  == A2  (harness deterministic)
A  == B   (in-process round trip OK)
A  == C2  (cross-process restore OK)
IDS 16 15 15 15 15 15 15 15 15 15 15 15 15 15 15
```

Blob sizes for an 8192-token prompt, q8_0 KV, ctx 16384:

| context | blob |
|---|---|
| target (27B) | 442,236,440 B (421.6 MiB) |
| MTP draft (Q4_0, 1 head) | 33,714,204 B (32.2 MiB) |

So one snapshot of this shape is ~454 MiB on disk before any compression or
per-prefix sharing. Draft context is ~7.6% of the target blob.

## Notes

* The state blob is per-seq (`seq_id 0`); with `--parallel 1` that is the
  whole context. Multi-slot units need one blob per active seq.
* `set_data_ext` accepts the blob into a **fresh** context of the same
  shape (same model, same KV types, same `ctx-size`); the draft context
  must also be restored, otherwise the MTP head has no KV and the first
  post-restore draft is garbage.
* Pitfalls hit while building the harness (kept in the test for reference):
  * the draft-mtp speculative impl enables the target's nextn embeddings in
    its constructor, so `common_speculative_init` must run **before** the
    prompt prefill, and the prompt batch must be fed to
    `common_speculative_process`;
  * a single `llama_decode` may not exceed `n_batch` - the prefill is
    chunked at `n_batch`, exactly like the server;
  * test binaries must carry `-Wl,--disable-new-dtags
    -Wl,-rpath,/opt/llama-fork/lib64`, otherwise the loader picks the stock
    `/usr/local/lib64/libllama*.so` (different commit, different
    `common_params` layout) and segfaults.

## Gate decision

T0.1 **PASS**. The L2 snapshot format can store exactly this blob pair
(target + draft) as payload. Proceed with Phase 1 as planned.
