#!/usr/bin/env bash
# Exercise the strict-write rules: restore-into-cold-backend + transactional publish.
set -u
API_KEY="$(cat ~/ollm-cache/.backend_key)"
H=(-H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" -H "X-Session-Id: strictwrite1")
echo "=== turn 1 (fresh session, backend cold after rebuild) ==="
curl -s --max-time 600 "${H[@]}" -d '{"messages":[{"role":"user","content":"Remember token OMEGA-55. Reply only: remembered"}],"max_tokens":300,"temperature":0,"stream":false}' http://127.0.0.1:1247/v1/chat/completions | python3 -c 'import json,sys; print(repr((json.load(sys.stdin)["choices"][0]["message"]["content"] or "")[:40]))'
sleep 4
echo "=== disk after publish ==="
ls -la ~/ollm-cache/slots/ | grep strictwrite1
echo "=== turn 2 recall ==="
curl -s --max-time 600 "${H[@]}" -d '{"messages":[{"role":"user","content":"Remember token OMEGA-55. Reply only: remembered"},{"role":"assistant","content":"remembered"},{"role":"user","content":"What token did I ask you to remember? Answer with just the token."}],"max_tokens":400,"temperature":0,"stream":false}' http://127.0.0.1:1247/v1/chat/completions | python3 -c 'import json,sys; print(repr((json.load(sys.stdin)["choices"][0]["message"]["content"] or "")[:60]))' 2>/dev/null || true
echo "=== proxy log (write-rule evidence) ==="
grep -E "strictwrite1|published|evict|restore" ~/ollm-cache/ollm.log | tail -12
