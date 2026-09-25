# AGENTS.md — oLLM

**Start here: read [`HANDOFF.md`](HANDOFF.md).** It is the live state of
this project — what's verified, what's next, which pitfalls have already
cost hours, and which decisions are settled (with their revisit triggers).
`README.md` is the product spec; `docs/design/` is the vendored deep design
(historical, superseded banner applies).

Hard rules for any agent:
- Backend container is **`llama-rocm2`**. Never try to start `llama-rocm-7.14` — it is dead with a corrupted overlay.
- Start/repair the stack with `~/ollm-cache/start-stack.sh`; verify with `bash ~/ollm-cache/strict-write-test.sh`.
- Never `pkill -f llama-server` over ssh (matches the ssh command itself, kills your session).
- All performance claims must cite a measured number + workload; see HANDOFF before quoting any figure.
