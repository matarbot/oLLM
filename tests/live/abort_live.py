#!/usr/bin/env python3
"""Live abort acceptance on box0 silicon via the proxy. Ritual #5.
1. start stream, read a few chunks, hang up
2. poll backend /slots: slot must go idle quickly (no zombie generation)
3. proxy log: stream pump finished disconnected=true, NO save for that turn

Endpoints resolve from env (same knobs as ops/common.sh).
"""
import json, os, time, urllib.request

PROXY_URL = os.environ.get("OLLM_PROXY_URL", "http://127.0.0.1:1247")
BACKEND_URL = os.environ.get("OLLM_BACKEND_URL", "http://127.0.0.1:1245")
OLLM_HOME = os.environ.get("OLLM_HOME", os.path.expanduser("~/ollm-cache"))

body = {"model": "qwen3.8", "temperature": 0, "max_tokens": 800, "stream": True,
        "messages": [{"role": "system", "content": "abort acceptance probe."},
                     {"role": "user", "content": "Write a very long detailed essay about every lighthouse in Denmark, at least 600 words."}]}

req = urllib.request.Request(f"{PROXY_URL}/v1/chat/completions",
                             data=json.dumps(body).encode(),
                             headers={"Content-Type": "application/json"}, method="POST")
t0 = time.time()
r = urllib.request.urlopen(req, timeout=300)
n = 0
for line in r:
    n += 1
    if n >= 5:
        break
r.close()
print(f"hung up after {n} chunks at {time.time()-t0:.1f}s")

BKEY = open(os.path.join(OLLM_HOME, ".backend_key")).read().strip()
BH = {"Authorization": f"Bearer {BKEY}"}
for i in range(20):
    time.sleep(0.5)
    try:
        req = urllib.request.Request(f"{BACKEND_URL}/slots", headers=BH)
        slots = json.load(urllib.request.urlopen(req, timeout=5))
        busy = [s["id"] for s in slots if s.get("is_processing")]
        if not busy:
            print(f"backend idle at {time.time()-t0:.1f}s after hangup")
            break
        print(f"  t+{time.time()-t0:.1f}s still processing: slots {busy}")
    except Exception as e:
        print("  poll err", str(e)[:60])
