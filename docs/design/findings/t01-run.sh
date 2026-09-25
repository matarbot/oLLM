#!/bin/bash
# T0.1 runner: compile tests/test-state-roundtrip.cpp inside the fork builder
# container, run the protocol modes in the fork runtime container, compare
# the 16-token continuations.
#
# Protocol (PHASE0-HANDOFF.md step 4):
#   A  gen        uninterrupted: prefill 8192, decode 16
#   A2 gen        control: proves the harness itself is deterministic
#   B  roundtrip  prefill, FLAGS_NONE snapshot of ctx_tgt + ctx_dft, seq_rm, restore in-process, decode 16
#   C  save+restore  snapshot to files, exit; new process restores into empty contexts, decode 16
# All four IDS lines must be byte-identical.
#
# Notes on this box (see findings/00-prefix-stability.md section 6):
# - containers cannot relabel the /home/rain bind mount (SELinux enforcing),
#   so every run uses --security-opt label=disable
# - the builder image has no ROCm runtime libs; compile there, run in the runtime image
# - MODES="A" limits the run to a subset of the five modes

set -u
cd "$(dirname "$0")/.." || exit 1          # -> docs/development/disk-cache/
REPO=$(cd ../../.. && pwd)                  # -> oLLM repo root
SHA=${1:-8a1a571ae}
OUT=${2:-$REPO/docs/development/disk-cache/findings/t01-raw-$SHA}
MODES=${MODES:-"A A2 B C1 C2"}
mkdir -p "$OUT"

BUILDER=localhost/llama-fork-builder:disk-cache-$SHA
RUNTIME=localhost/llama-fork:disk-cache-$SHA

build_image() { # $1 = tag (BUILDER | RUNTIME)
    local tag=$1 target=""
    [ "$tag" = "$BUILDER" ] && target="--target builder"
    ( cd "$REPO" && podman build $target \
      -f docs/development/disk-cache/container/Dockerfile.fork \
      --ignorefile docs/development/disk-cache/container/containerignore \
      --build-context patches=$HOME/source/setup/amd-strix-halo-toolboxes/toolboxes \
      --build-arg GIT_REF=disk-cache \
      --build-arg GIT_SHA=$SHA-dirty \
      --build-arg BUILD_JOBS=12 \
      -t "$tag" )
}

if ! podman image exists "$BUILDER"; then
    echo "builder image missing, building..."
    build_image "$BUILDER" || exit 1
fi
if ! podman image exists "$RUNTIME"; then
    echo "runtime image missing, building..."
    build_image "$RUNTIME" || exit 1
fi

# SELinux cannot relabel the bind mount on this box; scratch containers only
PODMAN_OPTS=(--device /dev/dri --device /dev/kfd
             --group-add video --group-add render
             --security-opt seccomp=unconfined
             --security-opt label=disable
             -v /home/rain:/home/rain)

compile_once() {
    podman run --rm "${PODMAN_OPTS[@]}" "$BUILDER" bash -c "
        set -e
        SRC=/home/rain/source/oLLM
        # link against the fork's own libs and force runtime resolution to them:
        # the stock toolbox image also ships /usr/local/lib64/libllama*.so (other
        # commit, other common_params layout) and would otherwise win the search
        LIBS=\$(find /opt/llama-fork-build -name 'lib*.so' ! -name '*.so.*' | tr '\n' ' ')
        if [ ! -x /home/rain/t01/test-state-roundtrip ] || [ \$SRC/tests/test-state-roundtrip.cpp -nt /home/rain/t01/test-state-roundtrip ]; then
          echo 'compiling test-state-roundtrip...'
          mkdir -p /home/rain/t01
          g++ -O2 -std=c++17 -o /home/rain/t01/test-state-roundtrip \
            \$SRC/tests/test-state-roundtrip.cpp \
            -I\$SRC/include -I\$SRC/common -I\$SRC/ggml/include \
            \$LIBS \
            -Wl,--disable-new-dtags -Wl,-rpath,/opt/llama-fork/lib64
        else
          echo 'binary up to date'
        fi
      " >> "$OUT/compile.log" 2>&1
    local rc=$?
    if [ $rc -ne 0 ]; then echo "!!! compile failed rc=$rc"; tail -30 "$OUT/compile.log"; return $rc; fi
    return 0
}

RUNNER() { # $1 = label, rest = mode args
    local label=$1; shift
    echo "=== $label ==="
    podman run --rm "${PODMAN_OPTS[@]}" "$RUNTIME" bash -c "
        set -e
        exec /home/rain/t01/test-state-roundtrip $*
      " 2>&1 | tee "$OUT/$label.log"
    local rc=${PIPESTATUS[0]}
    if [ $rc -ne 0 ]; then echo "!!! $label failed rc=$rc"; return $rc; fi
    return 0
}

MODEL_ARGS="--model /home/rain/models/qwen3.8-27b/Qwen3.8-27B-UD-Q4_K_XL.gguf \
  --spec-type draft-mtp --spec-draft-model /home/rain/models/qwen3.8-27b/MTP/mtp-Qwen3.8-27B-Q4_0.gguf \
  --spec-draft-n-max 3 --ctx-size 16384 --parallel 1 --n-gpu-layers -1 \
  --cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on --load-mode none \
  --batch-size 2048 --temp 0 --n-predict 16 --seed 0 --log-verbosity 1"

compile_once || exit 1
rm -rf /home/rain/t01/state

FAIL=0
for label in $MODES; do
    case $label in
        A)  RUNNER A  --mode=gen        $MODEL_ARGS --prompt-tokens 8192 || FAIL=1 ;;
        A2) RUNNER A2 --mode=gen        $MODEL_ARGS --prompt-tokens 8192 || FAIL=1 ;;
        B)  RUNNER B  --mode=roundtrip  $MODEL_ARGS --prompt-tokens 8192 || FAIL=1 ;;
        C1) RUNNER C1 --mode=save       $MODEL_ARGS --prompt-tokens 8192 --state-dir /home/rain/t01/state || FAIL=1 ;;
        C2) RUNNER C2 --mode=restore    $MODEL_ARGS --prompt-tokens 8192 --state-dir /home/rain/t01/state || FAIL=1 ;;
        *) echo "unknown mode $label"; FAIL=1 ;;
    esac
done

echo
echo "=== comparison ==="
HAVE_ALL=1
for f in A A2 B C2; do
    grep '^IDS' "$OUT/$f.log" > "$OUT/$f.ids" 2>/dev/null || { echo "$f: no IDS line"; HAVE_ALL=0; FAIL=1; }
done
if [ $HAVE_ALL -eq 1 ]; then
    if cmp -s "$OUT/A.ids" "$OUT/A2.ids"; then echo "A == A2 (harness deterministic)"; else echo "!!! A != A2: harness itself is nondeterministic - STOP, do not compare further"; FAIL=1; fi
    if cmp -s "$OUT/A.ids" "$OUT/B.ids";  then echo "A == B  (in-process round trip OK)";   else echo "!!! A != B: in-process round trip DIVERGES"; FAIL=1; fi
    if cmp -s "$OUT/A.ids" "$OUT/C2.ids"; then echo "A == C2 (cross-process restore OK)";   else echo "!!! A != C2: cross-process restore DIVERGES"; FAIL=1; fi
fi
echo
echo "--- first-divergence detail (if any) ---"
for f in A2 B C2; do
    if [ -f "$OUT/$f.ids" ] && ! cmp -s "$OUT/A.ids" "$OUT/$f.ids"; then
        paste -d' ' "$OUT/A.ids" "$OUT/$f.ids" | awk '{for(i=1;i<=NF;i+=2) if($i != $(i+1)) {print "first divergence at token " i/2 ": A=" $i "  '"$f"'=" $(i+1); exit}}'
        grep '^TEXT' "$OUT/A.log"  | head -1
        grep '^TEXT' "$OUT/$f.log" | head -1
    fi
done
echo
grep -h '^SIZES'  "$OUT/B.log"  "$OUT/C1.log" 2>/dev/null | sed 's/^/blob sizes: /'
grep -h '^BLOB'   "$OUT/C2.log" 2>/dev/null | sed 's/^/restored:  /'
echo
[ $FAIL -eq 0 ] && echo "VERDICT: PASS" || echo "VERDICT: FAIL"
exit $FAIL
