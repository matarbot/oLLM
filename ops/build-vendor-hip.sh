#!/usr/bin/env bash
# One-time (+ after submodule bumps): configure + build HIP llama-server
# from the vendored submodule (vendor/llama.cpp, branch ckpt-persist).
# Builds INSIDE the container (ROCm toolchain); home dir is bind-mounted so
# artifacts land on the host tree at vendor/llama.cpp/build-hip.
# Flags match the original build-hip cache: Release, GGML_HIP, gfx1151,
# server+tools on, UI/tests off.
# Poll for completion: tail -f $BUILD_LOG (BUILD_OK / BUILD_FAIL)
set -eu
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/common.sh"

podman ps --format "{{.Names}}" | grep -qx "$CONTAINER" \
  || { echo "container $CONTAINER not running (run ops/start-backend.sh first)"; exit 1; }

podman exec -d "$CONTAINER" bash -c "
  { cmake -S $SOURCE_DIR -B $BUILD_DIR \
      -DCMAKE_BUILD_TYPE=Release -DGGML_HIP=ON -DGGML_ROCM_ARCH=gfx1151 \
      -DLLAMA_BUILD_SERVER=ON -DLLAMA_BUILD_TOOLS=ON \
      -DLLAMA_BUILD_UI=OFF -DLLAMA_BUILD_TESTS=OFF \
    && cmake --build $BUILD_DIR -j\$(nproc) \
    && echo BUILD_OK || echo BUILD_FAIL; } >> $BUILD_LOG 2>&1
"
echo "build started detached -> $BUILD_LOG"
