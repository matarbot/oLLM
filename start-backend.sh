#!/usr/bin/env bash
# Relaunch llama-server inside a fresh llama-rocm2 container (same prod args).
# NOTE: the old llama-rocm-7.14 container's overlay is corrupted (missing
# merged/etc/passwd) and cannot be restarted — llama-rocm2 was created from
# the same image instead, but a plain `podman run` does NOT inherit the
# toolbox binds. /home/rain MUST be mounted or --slot-save-path dies with
# EACCES (it lives outside the container's own fs view otherwise).
set -eu
API_KEY="$(cat ~/ollm-cache/.backend_key)"

# one-time: recreate with the right binds if the container lacks /home/rain
if ! podman exec llama-rocm2 test -r /home/rain/ollm-cache 2>/dev/null; then
  podman rm -f llama-rocm2 2>/dev/null || true
  podman run -d --name llama-rocm2 \
    --network host --privileged \
    -v /home/rain:/home/rain \
    -v /dev:/dev \
    -v /run/udev:/run/udev:ro \
    docker.io/kyuz0/amd-strix-halo-toolboxes:rocm-7.14 sleep infinity >/dev/null
  sleep 2
fi

podman exec -d llama-rocm2 sh -c '/usr/local/bin/llama-server \
  --model /home/rain/models/lucebox-qwen/Qwen3.8-27B-UD-IQ4_XS.gguf \
  --alias qwen3.8 \
  --spec-type draft-mtp \
  --spec-draft-model /home/rain/models/qwen3.8-27b/MTP/mtp-Qwen3.8-27B-Q4_0.gguf \
  --spec-draft-n-max 3 \
  --host 0.0.0.0 --port 1245 \
  --ctx-size 786432 --parallel 3 \
  --n-gpu-layers -1 \
  --cache-type-k q8_0 --cache-type-v q8_0 \
  --cache-ram 32768 \
  --api-key "$API_KEY" \
  --flash-attn on \
  --slot-save-path /home/rain/ollm-cache/slots \
  --log-verbosity 3 > /home/rain/ollm-cache/server.log 2>&1'

for i in $(seq 1 120); do
  curl -s --max-time 2 http://127.0.0.1:1245/health 2>/dev/null | grep -q ok && { echo "backend ready poll $i"; exit 0; }
  sleep 3
done
echo "backend did NOT come up"; podman logs --tail 20 llama-rocm2 2>&1; exit 1
