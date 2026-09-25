#!/usr/bin/env bash
# Start llama-server backend + oLLM proxy on box0 (E-experiment harness).
set -eu
API_KEY="$(cat ~/ollm-cache/.backend_key 2>/dev/null || echo keyy)"
BACKEND_ARGS=(
  /usr/local/bin/llama-server
  --model /home/rain/models/lucebox-qwen/Qwen3.8-27B-UD-IQ4_XS.gguf
  --alias qwen3.8
  --spec-type draft-mtp
  --spec-draft-model /home/rain/models/qwen3.8-27b/MTP/mtp-Qwen3.8-27B-Q4_0.gguf
  --spec-draft-n-max 3
  --host 0.0.0.0 --port 1245
  --ctx-size 786432 --parallel 3
  --n-gpu-layers -1
  --cache-type-k q8_0 --cache-type-v q8_0
  --cache-ram 32768
  --api-key "$API_KEY"
  --flash-attn on
  --load-mode none
  --slot-save-path /home/rain/ollm-cache/slots
  --log-verbosity 4
)
if ! curl -s --max-time 2 http://127.0.0.1:1245/health | grep -q ok; then
  echo "starting backend..."
  podman exec -d llama-rocm-7.14 "${BACKEND_ARGS[@]}" 
  for i in $(seq 1 60); do
    curl -s --max-time 2 http://127.0.0.1:1245/health 2>/dev/null | grep -q ok && { echo "backend ready poll $i"; break; }
    sleep 3
  done
else
  echo "backend already up"
fi
pkill -x ollm 2>/dev/null || true
sleep 1
cd ~/source/oLLM
OLLM_BACKEND_URL=http://127.0.0.1:1245 \
OLLM_BACKEND_API_KEY="$API_KEY" \
OLLM_CACHE_DIR=/home/rain/ollm-cache/slots \
OLLM_BIND=127.0.0.1:1247 \
OLLM_CACHE_LIMIT_MB=2048 \
nohup ./target/debug/ollm > ~/ollm-cache/ollm.log 2>&1 &
sleep 2
echo "proxy health:"
curl -s --max-time 3 http://127.0.0.1:1247/health
echo
