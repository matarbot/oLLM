#!/usr/bin/env bash
# Start the full oLLM stack: backend (podman container, vendored binary) +
# proxy. Front door of ops/. Backend launch is delegated to start-backend.sh —
# the single source of truth for prod args.
set -eu
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/common.sh"

if ! curl -s --max-time 2 "$BACKEND_URL/health" | grep -q ok; then
  echo "backend down -> delegating to start-backend.sh"
  bash "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/start-backend.sh"
else
  echo "backend already up"
fi

pkill -x ollm 2>/dev/null || true
sleep 1
cd "$REPO"
OLLM_BACKEND_URL="$BACKEND_URL" \
OLLM_BACKEND_API_KEY="$(api_key)" \
OLLM_CACHE_DIR="$SLOTS_DIR" \
OLLM_BIND="$BIND" \
OLLM_CACHE_LIMIT_MB="$CACHE_LIMIT_MB" \
nohup ./target/debug/ollm > "$PROXY_LOG" 2>&1 &
sleep 2
echo "proxy health:"
curl -s --max-time 3 "$PROXY_URL/health"
echo
