# ops/ — build, launch, and teardown scripts

How to bring the box0 stack up, down, and rebuild it. All scripts source
`../common.sh` for paths and knobs (env vars override defaults there).
Runtime state (blobs, logs, backend key) lives in `$OLLM_HOME`
(default `~/ollm-cache`), never in the repo.

| script | what it does |
|---|---|
| `build-vendor-hip.sh` | Configure + build HIP llama-server from `vendor/llama.cpp` inside the container (gfx1151, Release). Detached; poll `$OLLM_HOME/build-vendor.log` for `BUILD_OK`. Run once, then after every submodule bump. |
| `start-backend.sh` | Ensure container `ollm-backend` exists with the required binds (`/home/rain` or `--slot-save-path` dies with EACCES), launch the vendored binary with prod flags, health-poll :1245. Idempotent. |
| `start-stack.sh` | Front door: health-probe backend, delegate to `start-backend.sh` if down, then (re)start the proxy :1247 with `cache_limit_mb`. |
| `stop-stack.sh` | Teardown: proxy first, then llama-server inside the container. Container keeps running. `pkill` hits ALL llama-servers in the container — announce on a live box. |

Quick start:

```bash
git clone --recurse-submodules https://github.com/matarbot/oLLM.git
cd oLLM
printf 'your-backend-key' > ~/ollm-cache/.backend_key   # never committed
bash ops/start-stack.sh        # builds nothing; expects binary — see below
# first time (or after bumping the submodule):
bash ops/build-vendor-hip.sh && tail -f ~/ollm-cache/build-vendor.log
```

Note: `llama-rocm-7.14` (the old container name) has a corrupted overlay —
never start it. Current stack runs `ollm-backend`.
