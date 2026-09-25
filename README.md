# oLLM

A thin, disk-cache-aware OpenAI-API proxy in front of a stock
[`llama-server`](https://github.com/ggml-org/llama.cpp).

llama.cpp can persist KV cache to disk — but only via operator-driven
`/slots/{id}?action=save|restore` with `--slot-save-path`. Upstream declined
to automate it ([#17107](https://github.com/ggml-org/llama.cpp/issues/17107):
*"mechanism server-side, policy client-side"*). **oLLM is that policy.**

It keeps a disk-backed forest of cached prompt prefixes, restores the deepest
matching blob into a slot *before* forwarding a request, and publishes slot
state back to disk transactionally when the turn completes. A device shipped
with a warmed cache serves its first real message with a restore (~13 ms
measured for a 153 MB hybrid snapshot) instead of a cold prefill (~6.5 s for
a ~10k-token system prompt) — no RAM-tier cache required, which matters on
unified-memory (UMA) devices where the cache competes with the model.

## Quick start

```bash
cargo build --release

OLLM_BACKEND_URL=http://127.0.0.1:1245 \
OLLM_BACKEND_API_KEY=*** \
OLLM_CACHE_DIR=/var/lib/ollm/cache \
OLLM_BIND=127.0.0.1:1247 \
OLLM_CACHE_LIMIT_MB=20480 \
./target/release/ollm
```

Point your harness at `http://127.0.0.1:1247/v1`. The backend **must** be
launched with `--slot-save-path <same dir as OLLM_CACHE_DIR>` (oLLM steers
slots via `id_slot` and saves/restores by filename).

Send a stable `X-Session-Id` header (configurable via `OLLM_SESSION_HEADER`)
to get per-session cache accumulation. Without it, sessions are keyed by a
prompt-content hash — correct, but every new-turn prefix is a new key, so
blobs churn instead of accumulating.

## Configuration

| env | default | meaning |
|---|---|---|
| `OLLM_BACKEND_URL` | `http://127.0.0.1:1245` | llama-server base URL |
| `OLLM_BACKEND_API_KEY` | *(none)* | bearer key for the backend |
| `OLLM_CACHE_DIR` | `~/.ollm/cache` | blob directory; must equal backend `--slot-save-path` |
| `OLLM_BIND` | `0.0.0.0:1247` | proxy listen address |
| `OLLM_CACHE_LIMIT_MB` | `51200` | hard cap; eviction is LRU by last-use, not insertion age |
| `OLLM_SESSION_HEADER` | `x-session-id` | header carrying a stable conversation id |

## Endpoints

- `POST /v1/chat/completions` — cache-aware forward (slot steering, streaming-safe)
- `GET /health` — cache size, backend, backend signature
- everything else — verbatim passthrough (`/props`, `/slots`, …)

## Strict write rules (the invariants)

These are the rules that make the cache boring — nothing falls through the
cracks, and no reader can ever observe a half-written state.

1. **The write gate.** Prefix-match/lookup takes an `RwLock` *shared*; every
   publish takes it *exclusive*. A matcher never decides against a forest
   that is mid-write, and no new match starts during a publish. Decoding
   runs outside the gate — a long generation never blocks the world.
2. **Transactional publish.** Save goes to `<blob>.save` scratch → `fsync`
   file and directory → atomic `rename` into place. A visible blob is always
   complete; a torn blob is structurally impossible. A crash mid-save leaves
   only scratch garbage, never a poisoned cache entry.
3. **Dirty-slot registry.** oLLM tracks which session owns each slot and
   whether its KV is unpublished (*dirty*). A dirty slot is never
   restored-over and never stolen. Write-on-eviction: before oLLM reuses a
   slot holding another conversation's state, the victim is published to disk
   first; if the publish fails, the slot is not taken.
4. **Compatibility gate.** Blob filenames embed a backend signature — a
   fingerprint of model path, slots, context, cache types, template, draft
   config taken from `/props` at startup. A blob saved under one backend can
   never restore into another: mismatched KV restore is *silently wrong* on
   hybrid models, so mismatch means cold prefill, always.
   `session__<backend_sig>.bin`.

## Cache model

v0 keys blobs by `session__<backend_sig>` and evicts by `last_access`
(mtime) under the byte cap — deliberately the simplest scoring that works.

The target model (v1, "forest") treats the cache as a forest of prefix
trees: nodes are KV snapshots at token positions, a node's parent is the
longest available cached prefix it extends, admission does longest-prefix
match, eviction peels LRU leaves first, and frequently-hit spans get
promoted into shared trunks while dominated siblings are pruned. On hybrid
models a node can only be *born* where a save naturally ends (end-of-message
span or crafted `n_predict=0` boundary) — recurrent state cannot be sliced.
The prefix-hash key scheme (chained content hashes rooted at a compat key)
is specified in [`docs/design/design.md`](docs/design/design.md) §3.

## Verified on hardware

Ryzen AI Max+ 395 (gfx1151), Qwen3.8-27B IQ4_XS + MTP draft, llama.cpp
b10627, ROCm 7.14:

- save 16 ms / 153 MB, restore 13 ms — KV + recurrent state intact (E1)
- persistence proof: recall after *hard* backend restart, RAM cache gone
- streaming: save driven by pump completion (drains to EOF even when the
  client disconnects), TTFT ~128 ms hot
- RAM-tier prefix sharing measured at ~670 MB RAM per cached prompt —
  the reason disk is the tier for UMA devices (E7)

Blob size floor is ~153 MB/slot (recurrent state) + ~34 KB/token (attention)
on this hybrid arch — full-slot FLAGS_NONE snapshots; llama.cpp cannot save
partial states.

## docs/design/ — vendored, historical

`docs/design/` is vendored from the pre-proxy design effort (a C++ llama.cpp
fork that never shipped `--cache-disk`) at commit `a43f9c40e`, branch
`disk-cache`. It is retained as the detailed reference for the data model,
crash/race analysis (§6), and compatibility gate (§7) — the parts that carry
over unchanged. Where it differs from this README: it targets an in-server
tier (`--cache-disk`); oLLM supplies that policy from outside the server
instead. See `docs/design/README.md` for its own reading order.

## Repo scripts

- `smoke.sh` — round-trip + recall through the proxy
- `persistence-proof.sh` — kill backend hard, restore, recall
- `stream-test.sh` — streaming round-trip + disconnect behavior
- `start-stack.sh` / `start-backend.sh` — box0 experiment harness (container + server + proxy)
- `strict-write-test.sh` — invariants 1–4 end-to-end

## License

TBD — see repo settings.
