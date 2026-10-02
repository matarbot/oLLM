#!/usr/bin/env bash
# Exercise the strict-write rules: restore-into-cold-backend + transactional
# publish. Verification ritual #1 — run after any stack change.
# Expected: 'remembered' -> blob+.ckpt pair on disk -> 'OMEGA-55'.
set -u
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/../../ops/common.sh"
API_KEY="$(api_key)"
H=(-H "Authorization: Bearer ***" -H "Content-Type: application/json" -H "X-Session-Id: strictwrite1")
echo "=== turn 1 (fresh session, backend cold after rebuild) ==="
curl -s --max-time 600 "${H[@]}" -d '{"messages":[{"role":"user","content":"Remember token OMEGA-55. Reply only: remembered"}],"max_tokens":300,"temperature":0,"stream":false}' "$PROXY_URL/v1/chat/completions" | python3 -c 'import json,sys; print(repr((json.load(sys.stdin)["choices"][0]["message"]["content"] or "")[:40]))'
sleep 4
echo "=== disk after publish ==="
ls -la "$SLOTS_DIR"/ | grep strictwrite1
echo "=== turn 2 recall ==="
curl -s --max-time 600 "${H[@]}" -d '{"messages":[{"role":"user","content":"Remember token OMEGA-55. Reply only: remembered"},{"role":"assistant","content":"remembered"},{"role":"user","content":"What token did I ask you to remember? Answer with just the token."}],"max_tokens":400,"temperature":0,"stream":false}' "$PROXY_URL/v1/chat/completions" | python3 -c 'import json,sys; print(repr((json.load(sys.stdin)["choices"][0]["message"]["content"] or "")[:60]))' 2>/dev/null || true
echo "=== proxy log (write-rule evidence) ==="
grep -E "strictwrite1|published|evict|restore" "$PROXY_LOG" | tail -12
