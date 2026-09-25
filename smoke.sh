#!/usr/bin/env bash
# oLLM proxy smoke test — passthrough + first conversation + save + restart persistence
set -u
PX=http://127.0.0.1:1247
echo "=== 1. passthrough /props (auth check) ==="
curl -s --max-time 5 $PX/props | head -c 160; echo; echo

echo "=== 2. chat via proxy (session first-contact, non-stream) ==="
curl -s --max-time 90 $PX/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Session-Id: first-contact' \
  -d '{"model":"qwen3.8","messages":[{"role":"user","content":"Remember the magic word is KIWI-77. Reply only: remembered."}],"max_tokens":120,"temperature":0,"reasoning_effort":"low"}' \
  | head -c 300; echo; echo

echo "waiting for async save..."
for i in $(seq 1 40); do
  [ -f /home/rain/ollm-cache/slots/first-contact.bin ] && echo "blob exists after ~${i} polls" && break
  sleep 1
done
ls -la /home/rain/ollm-cache/slots/ | grep first-contact || echo "NO BLOB YET"

echo
echo "=== 3. continuation via proxy (same session) ==="
curl -s --max-time 90 $PX/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Session-Id: first-contact' \
  -d '{"model":"qwen3.8","messages":[{"role":"user","content":"Remember the magic word is KIWI-77. Reply only: remembered."},{"role":"assistant","content":"remembered"},{"role":"user","content":"What was the magic word? Reply with just the word."}],"max_tokens":200,"temperature":0,"reasoning_effort":"low"}' \
  | python3 -c 'import json,sys; j=json.load(sys.stdin); m=j["choices"][0]["message"]; print("content:", repr(m.get("content","")[:60])); print("cached:", j.get("usage",{}).get("prompt_tokens_details",{}).get("cached_tokens"))'
