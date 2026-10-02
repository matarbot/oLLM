#!/usr/bin/env bash
# Take the stack down: proxy first (so nothing hits the backend), then
# llama-server inside the container. The container itself keeps running
# (sleep infinity); remove it explicitly with `podman rm -f ollm-backend`.
# NOTE: pkill hits ALL llama-server processes in the container — fine
# pre-production (no prod consumers); announce before using on a live box.
set -eu
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/common.sh"

pkill -x ollm 2>/dev/null && echo "proxy stopped" || echo "proxy not running"
if podman ps --format "{{.Names}}" | grep -qx "$CONTAINER"; then
  podman exec "$CONTAINER" pkill -f llama-server 2>/dev/null \
    && echo "backend llama-server stopped" || echo "no llama-server in container"
else
  echo "container $CONTAINER not running"
fi
