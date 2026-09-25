# T0.3 — real long-prompt pp baseline

Date: 2026-08-28. Server: production `qwen3.8-27b` unit on 127.0.0.1:1245,
untouched flags (`--ctx-size 786432 --parallel 3 --n-gpu-layers -1
--cache-type-k q8_0 --cache-type-v q8_0 --cache-ram 32768 --flash-attn on
--load-mode none`, MTP draft `--spec-type draft-mtp --spec-draft-n-max 3`).
No server restarts were needed or performed.

## Method

* `POST /v1/chat/completions`, `stream false`, `temperature 0`,
  `max_tokens 96`; read `timings.prompt_per_second`, `timings.cache_n`,
  `usage.prompt_tokens` (exact count from the server's own tokenizer).
* Prompts are hermes-shaped: the **real captured system prompt** (21644
  chars) and **real 20 tool definitions** from the T0.0 capture, plus short
  turns and a deterministic filler "document" (script:
  `findings/t03-pp-baseline.py`, filler = pseudo-technical section lines,
  seed-parameterised).
* **Salt trick**: every run prepends a unique one-line salt to the system
  prompt. The server's L1 prompt cache (`--cache-ram 32768`) keys on prompt
  prefix, so without the salt, runs 2+ would silently serve the shared
  system+tools prefix from L1 (`cache_n > 0`) and the "cold" number would be
  wrong. With the salt, every run is a true cold prefill - verified:
  `cache_n = 0` on all 12 requests. Cost: ~10 tokens per run, negligible.
* Two runs per target length, different filler seeds.

## Shapes (labelled per row)

| shape | content | used for |
|---|---|---|
| `full` | real system + 20 tools + 2 short turns + filler | 32k, 64k (and calibration) |
| `notools` | real system, no tools, 2 short turns + filler | 8k |
| `minimal` | short system, no tools, 2 short turns + filler | 2k |

The `full` shape's base (no filler) is **15705 tokens** - a real hermes turn
already exceeds the 8k target, so 8k and 2k use reduced shapes. The shape is
part of the label, not a footnote.

## Results

| target | shape | run A tok | run B tok | pp A (t/s) | pp B (t/s) | pp mean | prefill A (s) | prefill B (s) |
|---|---|---|---|---|---|---|---|---|
| 2k | minimal | 2076 | 2069 | 276.6 | 273.1 | **274.9** | 7.50 | 7.58 |
| 8k | notools | 8254 | 8253 | 287.4 | 287.1 | **287.3** | 28.72 | 28.75 |
| 32k | full | 33619 | 33624 | 240.1 | 240.1 | **240.1** | 140.02 | 140.06 |
| 64k | full | 67926 | 67905 | 189.9 | 190.0 | **190.0** | 357.66 | 357.32 |

Calibration runs (full shape):

| run | tokens | pp (t/s) | prefill (s) |
|---|---|---|---|
| base, no filler | 15705 | 274.8 | 57.15 |
| base + 20000 filler chars | 22604 | 260.6 | 86.73 |

Filler ratio: 6899 tokens / 20000 chars = 0.345 tok/char (used to size the
other runs; landed within +1.3% to +3.6% of target).

## Reading

* pp is roughly flat (275-290 t/s) up to ~8k, then degrades: 261 at 22.6k,
  240 at 33.6k, **190 at 68k** (5.26 ms/token at the top end).
* A realistic hermes turn (15.7k tokens, `full` shape) costs **57 s of
  prefill** on this unit. This is the number the disk cache competes with:
  a ~15.7k-token snapshot is ~675 MiB (153 MiB constant + ~34 KiB/token,
  per design §4), and even a 500 MB/s read is ~1.4 s - a **40x** saving at
  realistic size, and 6 min vs ~20 s at 64k (~1.9 GiB snapshot, 180x at
  1 GB/s). The earlier "prefill might win at small N" intuition is wrong on
  this box: even at 2k the snapshot (~220 MiB) reads in well under a second
  against 7.5 s of prefill. Disk wins at every measured length; the
  `--cache-disk-pp-tts` threshold is about *when disk IO is cheap enough*,
  not about beating a prefill that is never cheap here.
* Decode during these runs: 96 tokens in ~13.4 s (7.1 t/s) with MTP
  (`draft_n 12, draft_n_accepted 9` in the calibration response) - not part
  of the pp number (`prompt_ms` excludes decode).

## Rows to hand to the principal for `~/source/setup/benchmark.md`

(That repo is not editable from here - its AGENTS.md says so.)

```
2026-08-28  qwen3.8-27b (prod unit, ROCm 7.14, MTP on, flash-attn on,
            ctx 786432/parallel 3, KV q8_0)  cold prefill, T0.3:
  2k   (minimal shape, 2076 tok)   pp 274.9 t/s   7.5 s
  8k   (notools shape, 8254 tok)   pp 287.3 t/s   28.7 s
  32k  (full hermes shape, 33619)  pp 240.1 t/s   140.0 s
  64k  (full hermes shape, 67926)  pp 190.0 t/s   357.7 s
  base (full hermes shape, 15705)  pp 274.8 t/s   57.1 s   <- real hermes turn
```

## Side effects

* 12 salted L1 entries were admitted to the production unit's 32 GiB
  prompt cache, totalling ~285k tokens (~9.5 GiB). They are LRU-evictable
  by normal hermes traffic; nothing persistent was changed.
* No unit, image, or config changes. The only production interaction was
  12 normal API requests.
