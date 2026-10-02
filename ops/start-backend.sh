#!/usr/bin/env bash
# Relaunch llama-server inside the ollm-backend container (canonical prod args).
# Binary: vendored submodule build (vendor/llama.cpp/build-hip, ckpt-persist).
# NOTE: llama-rocm-7.14's overlay is corrupted (missing merged/etc/passwd) —
# NEVER start it. A plain `podman run` does NOT inherit the toolbox binds;
# /home/rain MUST be mounted or --slot-save-path dies with EACCES.
set -eu
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/common.sh"
API_KEY="$(api_key)"
mkdir -p "$SLOTS_DIR"

if ! podman ps --format "{{.Names}}" | grep -qx "$CONTAINER"; then
  if ! podman inspect "$CONTAINER" >/dev/null 2>&1; then
    echo "container $CONTAINER missing -> creating with required binds"
    podman run -d --name "$CONTAINER" \
      --network host --privileged \
      -v /home/rain:/home/rain \
      -v /dev:/dev \
      -v /run/udev:/run/udev:ro \
      "$IMAGE" sleep infinity >/dev/null
    sleep 2
  else
    podman start "$CONTAINER" >/dev/null
  fi
fi

if [ ! -x "$SERVER_BIN" ]; then
  echo "vendor binary missing — build it first: bash ops/build-vendor-hip.sh" >&2
  exit 1
fi

podman exec -d "$CONTAINER" bash -c "$SERVER_BIN \
  --model $MODEL \
  --alias qwen3.8 \
  --spec-type draft-mtp \
  --spec-draft-model $DRAFT_MODEL \
  --spec-draft-n-max 3 \
  --host 0.0.0.0 --port 1245 \
  --ctx-size 786432 --parallel 3 \
  --n-gpu-layers -1 \
  --cache-type-k q8_0 --cache-type-v q8_0 \
  --cache-ram 32768 \
  --api-key \"$API_KEY\" \
  --flash-attn on \
  --slot-save-path $SLOTS_DIR \
  --log-verbosity 3 > $BACKEND_LOG 2>&1"

for i in $(seq 1 120); do
  curl -s --max-time 2 "$BACKEND_URL/health" 2>/dev/null | grep -q ok && { echo "backend ready poll $i"; exit 0; }
  sleep 3
done
echo "backend did NOT come up"; podman logs --tail 20 "$CONTAINER" 2>&1; exit 1
