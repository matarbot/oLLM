#!/usr/bin/env python3
"""T0.3: cold prefill pp baseline on the production 27B unit (port 1245).

Method (see findings/03-pp-baseline.md):
- prompts are hermes-shaped: real captured system prompt + 20 real tool
  definitions + short turns + deterministic filler document;
- every run gets a unique "salt" line prepended to the system prompt so no
  run can hit the server's L1 prompt cache (all runs measure a true cold
  prefill, no server restarts needed);
- temperature 0, max_tokens 96, stream false; read timings.prompt_per_second
  and usage.prompt_tokens (exact count from the server's tokenizer);
- two runs per target length with different filler seeds.

Shapes:
- full   = system + tools + 2 short turns (+ filler)        [8k/32k/64k]
- notools= system + 2 short turns, no tools (+ filler)      [8k fallback]
- minimal= short system + 2 short turns, no tools (+ filler)[2k]
"""
import json, sys, time, urllib.request

URL = "http://127.0.0.1:1245/v1/chat/completions"
KEY = "keyy"
CAPTURE = "/tmp/t00-chat-both.json"

def filler(seed, n_chars, per_line=110):
    """Deterministic pseudo-technical text; ~stable tokens/char ratio."""
    out, i, n = [], 0, 0
    while n < n_chars:
        line = (f"Section {seed}.{i//400}.{i%400}: the latency of component {(seed+i)%17} under "
                f"load profile P{(seed+i)%9} measured {((seed*31+i*7)%9000)/100:.2f} ms at depth "
                f"{(seed+i)%128}, with a cache hit ratio of {((seed*13+i*11)%10000)/100:.1f} percent "
                f"and slab index {(seed+i)%31}.")
        out.append(line)
        n += len(line) + 1
        i += 1
    return "\n".join(out)[:n_chars]

def make_prompt(shape, target_chars, salt, seed):
    cap = json.load(open(CAPTURE))["t2"]
    system = cap["messages"][0]["content"]
    tools = cap.get("tools")
    msgs = [{"role": "system", "content": salt + "\n" + system}]
    msgs.append({"role": "user", "content":
        "I am going to reference a technical document. When I ask, answer from it. "
        "Reply with exactly: received\n\n<document>\n" + filler(seed, target_chars) + "\n</document>"})
    msgs.append({"role": "assistant", "content": "received"})
    msgs.append({"role": "user", "content": "What was the slab index in section 3.14.10?"})
    if shape == "minimal":
        msgs = [{"role": "system", "content": salt + "\nYou are a technical assistant answering questions about a document."},
                {"role": "user", "content": "<document>\n" + filler(seed, target_chars) + "\n</document>\nSlab index of section 3.14.10?"}]
        tools = None
    elif shape == "notools":
        tools = None
    body = {"model": "qwen3.8", "messages": msgs, "tools": tools,
            "stream": False, "temperature": 0, "max_tokens": 96}
    return body

def run(body):
    req = urllib.request.Request(URL, data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {KEY}"})
    t0 = time.time()
    r = json.load(urllib.request.urlopen(req))
    wall = time.time() - t0
    tim, us = r["timings"], r["usage"]
    return {"prompt_tokens": us["prompt_tokens"], "cached": tim["cache_n"],
            "pp": tim["prompt_per_second"], "prompt_ms": tim["prompt_ms"],
            "wall_s": round(wall, 2), "completion_tokens": us["completion_tokens"]}

def main():
    # argv: target_chars salt seed shape
    target, salt, seed, shape = int(sys.argv[1]), sys.argv[2], int(sys.argv[3]), sys.argv[4]
    res = run(make_prompt(shape, target, salt, seed))
    print(json.dumps(res))

if __name__ == "__main__":
    main()
