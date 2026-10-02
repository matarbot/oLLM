#!/usr/bin/env bash
# Streaming test through oLLM: fresh session, stream=true, two turns.
# Ritual #4. Watch proxy log: (turn 1) no restore, pump finished, saved;
# (turn 2) slot hot, TTFT, saved again.
set -u
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../../ops/common.sh"
PX="$PROXY_URL"
H='Content-Type: application/json'

echo "=== turn1 (stream, session stream-test) ==="
curl -sN --max-time 120 $PX/v1/chat/completions -H "$H" -H 'X-Session-Id: stream-test' \
 -d '{"model":"qwen3.8","messages":[{"role":"user","content":"Remember: launch code is THETA-9. Reply only: acknowledged."}],"max_tokens":120,"temperature":0,"reasoning_effort":"low","stream":true}' \
 | head -c 240; echo; echo

echo "waiting for post-stream save..."
for i in $(seq 1 30); do
  grep -q 'session="stream-test".*saved\|saved.*stream-test' "$PROXY_LOG" && echo "saved after ~${i}s" && break
  sleep 1
done

echo "=== turn2 (stream, same session, recall) ==="
curl -sN --max-time 120 $PX/v1/chat/completions -H "$H" -H 'X-Session-Id: stream-test' \
 -d '{"model":"qwen3.8","messages":[{"role":"user","content":"Remember: launch code is THETA-9. Reply only: acknowledged."},{"role":"assistant","content":"acknowledged"},{"role":"user","content":"What is the launch code? Reply with just the code."}],"max_tokens":200,"temperature":0,"reasoning_effort":"low","stream":true}' \
 | grep -o '"content":"[^"]*"' | tail -3; echo

echo "=== proxy log (stream-test) ==="
grep "stream-test" "$PROXY_LOG" | tail -12
