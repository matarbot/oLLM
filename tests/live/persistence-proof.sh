#!/usr/bin/env bash
# Persistence proof: backend was HARD-restarted (all RAM cache gone).
# oLLM must restore session 'first-contact' from disk and recall KIWI-77.
# Ritual #3 — run AFTER: ops/stop-stack.sh && ops/start-stack.sh,
# with the first-contact blob left behind by smoke.sh.
set -u
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../../ops/common.sh"
PX="$PROXY_URL"

echo "== slots fresh (all empty after restart) =="
curl -s --max-time 5 $PX/slots -H "Authorization: Bearer $(api_key)" | python3 -m json.tool | grep -E '"id"|processing'

echo "== disk blobs available =="
ls -la "$SLOTS_DIR"/*.bin | awk '{print $5, $9}'

echo "== cold continuation via proxy (should trigger disk restore) =="
/usr/bin/time -f "TTFT-ish wall: %e s" curl -s --max-time 120 $PX/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Session-Id: first-contact' \
  -d '{"model":"qwen3.8","messages":[{"role":"user","content":"Remember the magic word is KIWI-77. Reply only: remembered."},{"role":"assistant","content":"remembered"},{"role":"user","content":"What was the magic word? Reply with just the word."}],"max_tokens":200,"temperature":0,"reasoning_effort":"low"}' \
  | python3 -c 'import json,sys
j=json.load(sys.stdin)
m=j["choices"][0]["message"]
rc=m.get("reasoning_content") or ""
c=m.get("content") or ""
print("content:", repr(c[:60]))
print("PERSISTENCE VERIFIED:", "KIWI-77" in (c + rc))'

echo "== proxy log (restore evidence) =="
grep -E "restored|saved" "$PROXY_LOG" | tail -4
