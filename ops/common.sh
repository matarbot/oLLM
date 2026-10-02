#!/usr/bin/env bash
# Shared paths for all oLLM ops/ and tests/live/ scripts. Source:
#   . "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../ops/common.sh"
# REPO is resolved symlink-safe from this file's location, so the stack runs
# from any checkout: git clone --recurse-submodules && bash ops/start-stack.sh
#
# Runtime state (blobs, logs, backend key) lives OUTSIDE the repo in
# OLLM_HOME (default ~/ollm-cache) — the key is a secret, never committed.

_self() {
  local p="${BASH_SOURCE[0]}"
  while [ -L "$p" ]; do p="$(readlink "$p")"; done
  cd "$(dirname "$p")/../" && pwd
}
export REPO="$(_self)"

export OLLM_HOME="${OLLM_HOME:-$HOME/ollm-cache}"
export BACKEND_KEY_FILE="$OLLM_HOME/.backend_key"
export SLOTS_DIR="$OLLM_HOME/slots"
export PROXY_LOG="$OLLM_HOME/ollm.log"
export BACKEND_LOG="$OLLM_HOME/server.log"
export BUILD_LOG="$OLLM_HOME/build-vendor.log"

export CONTAINER="${OLLM_CONTAINER:-ollm-backend}"
export IMAGE="${OLLM_IMAGE:-docker.io/kyuz0/amd-strix-halo-toolboxes:rocm-7.14}"

export SOURCE_DIR="$REPO/vendor/llama.cpp"
export BUILD_DIR="$SOURCE_DIR/build-hip"
export SERVER_BIN="$BUILD_DIR/bin/llama-server"

export BACKEND_URL="${OLLM_BACKEND_URL:-http://127.0.0.1:1245}"
export BIND="${OLLM_BIND:-0.0.0.0:1247}"
export PROXY_URL="${OLLM_PROXY_URL:-http://127.0.0.1:1247}"
export CACHE_LIMIT_MB="${OLLM_CACHE_LIMIT_MB:-102400}"

export MODEL="${OLLM_MODEL:-/home/rain/models/lucebox-qwen/Qwen3.8-27B-UD-IQ4_XS.gguf}"
export DRAFT_MODEL="${OLLM_DRAFT_MODEL:-/home/rain/models/qwen3.8-27b/MTP/mtp-Qwen3.8-27B-Q4_0.gguf}"

api_key() { cat "$BACKEND_KEY_FILE" 2>/dev/null || echo keyy; }
