# tests/live/ — runtime test harnesses

Acceptance probes that run **against a live stack** (container up, proxy on
:1247, real model). They are not `cargo test`: they need box0 silicon, the
27B model, and the vendored HIP build. Unit tests stay in the Rust tree
(`cargo test`).

Run order after any stack change (matches the HANDOFF verification ritual):

| # | script | proves |
|---|---|---|
| 1 | `strict-write-test.sh` | cold-backend restore + transactional publish: `remembered` → blob+`.ckpt` pair on disk → recall `OMEGA-55` |
| 2 | `smoke.sh` | passthrough auth, first-contact conversation, async save (blob appears), same-session continuation with cached tokens |
| 3 | `persistence-proof.sh` | run AFTER `ops/stop-stack.sh` + backend restart: restores `first-contact` from disk, recalls `KIWI-77` |
| 4 | `stream-test.sh` | streaming round-trip, post-stream save, same-session recall |
| 5 | `abort_live.py` | client hang-up ABORTS backend generation: backend idle <2 s, no zombie, no save for that turn |
| 6 | `parallel_similar.py` | 3 parallel sessions sharing an ~8k-token base: thread 1 cold, threads 2+ hit RAM prefix cache (documents the cross-session gap block-sharing closes) |

All scripts source `ops/common.sh` (paths, URLs, ports); env vars override
defaults there. Sessions use fixed names and leak blobs — that is intentional,
LRU handles it; delete `$OLLM_HOME/slots/<session>*` to reset.
