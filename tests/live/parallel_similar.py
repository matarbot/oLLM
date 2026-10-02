#!/usr/bin/env python3
"""Parallel-similar-prompts test against the proxy. Ritual #6.
Three SEPARATE sessions (distinct system prompts -> distinct conv- keys ->
distinct blobs) that share a long base prompt. Each lands on its own slot.
Probe: does the backend RAM prefix cache share the common prefix across the
three slots? Expect: first thread prefills the base (~cold), threads 2+
hit the shared prefix (cached_tokens>0). Same base, different tail token
per thread so recall stays attributable.
Documents the cross-session disk gap that block-sharing (v1) closes.

Endpoint resolves from env (same knob as ops/common.sh). No X-Session-Id
headers: the proxy pins header-less sessions per conversation-key.
"""
import json, os, time, threading, urllib.request

PROXY_URL = os.environ.get("OLLM_PROXY_URL", "http://127.0.0.1:1247")
BASE = ("The harbor master kept a careful ledger of every ship that passed "
        "the narrow strait, noting the hour, the wind, and the cargo. " * 120)  # ~8k tok base

def session_turn(tag, sys_extra, tail):
    sysp = f"You are a precise assistant. Thread {tag}. {sys_extra} Date 2026-09-30."
    msgs = [{"role": "system", "content": sysp},
            {"role": "user", "content": BASE + f"\n\nGiven the ledger above, the keyword is {tail}. Reply only: {tail}"}]
    body = {"model": "qwen3.8", "messages": msgs, "max_tokens": 40, "temperature": 0, "stream": True}
    req = urllib.request.Request(f"{PROXY_URL}/v1/chat/completions", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    t0 = time.time(); ttft = None; n = 0
    try:
        r = urllib.request.urlopen(req, timeout=600)
        for line in r:
            if ttft is None: ttft = time.time() - t0
            n += 1
        return {"tag": tag, "ttft": round(ttft, 2), "dt": round(time.time()-t0, 2)}
    except Exception as e:
        return {"tag": tag, "err": str(e)[:80]}

threads = []
results = {}
def run(t):
    results[t[0]] = session_turn(*t)

specs = [("ALPHA", "focuses on maritime signals", "ALPHA"),
         ("BRAVO", "focuses on weather patterns", "BRAVO"),
         ("CHARLIE", "focuses on cargo manifests", "CHARLIE")]

# fire all three at once
t0 = time.time()
ths = [threading.Thread(target=run, args=(s,)) for s in specs]
for th in ths: th.start()
for th in ths: th.join()
print(f"three parallel sessions, wall={time.time()-t0:.1f}s")
for k in ("ALPHA", "BRAVO", "CHARLIE"):
    print(" ", results.get(k))
