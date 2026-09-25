# Test container for the patched fork (recipe — **not yet executed**)

Answers one question: how do we run *our* llama.cpp as the real server, on the real GPU,
without touching the production images? Base the test image on **kyuz0's own runtime
image**, compile the fork in a builder stage, and never modify the base. Recovery is
"stop/delete our container and our tag"; the toolboxes and `docker.io/kyuz0/...` stay
pristine.

⚠ **Status: recipe reviewed, no build has been run from this file yet.** The podman
mechanics it depends on are verified (see table in [`../HANDOFF.md`](../HANDOFF.md) §2); the
ROCm dev-package install and the HIP compile are not. Read `../HANDOFF.md` §2's two
"will bite you" items first — one of them is a swap-storm hazard on this box.

## Why not simpler options

* *install a compiler into the running toolbox*: the runtime toolbox has no `dnf`
  (`microdnf`→`dnf5` exists but the image is fedora-minimal-based and has no dev headers),
  and `refresh-toolboxes.sh` **deletes and recreates** the toolboxes, so anything installed
  there is transient. Don't turn a runtime container into a build box.
* *build on the host*: no `gcc`/`cmake`/`ninja` at all.
* *retag over the kyuz0 image*: destroys the recovery path. Never.

## Layout we end up with

| thing | value | why |
|---|---|---|
| base image | `docker.io/kyuz0/amd-strix-halo-toolboxes:rocm-7.14` | runtime ROCm/firmware identical to production |
| our tag | `localhost/llama-fork:<branch>-<shortsha>` | `localhost/` can never shadow a Docker Hub image |
| patched install dir | `/opt/llama-fork/{bin,lib,lib64}` | the image's stock `/usr/local/bin/llama-server` stays intact ⇒ A/B in one container |
| `PATH` | `/opt/llama-fork/bin` prepended | `llama-server` resolves to the patched build by default |
| test container | `llama-fork` | **never** `llama-flash-next` / `llama-rocm-7.14` (the units + refresh script key on those names) |
| test port | `1246` | production owns `1245` (one server per port) |
| cache dir for tests | `~/.cache/llama-fork/` (root; per-unit subdir appended) | keeps experiments out of the production namespace — production's root is `~/.cache/llama-server/` (`design.md` §6.10) |

## Step 1 — generate the Dockerfile

We deliberately **inherit the ROCm repo definition and the cmake flags verbatim from
upstream's Dockerfile** instead of retyping them: parity then cannot drift, and if AMD moves
the repo we pick it up with the submodule.

```sh
TB=~/source/setup/amd-strix-halo-toolboxes/toolboxes
OUT=$(mktempd)/Dockerfile.fork

# ROCm repo heredoc block (the `RUN <<'EOF' ... EOF` stanza) verbatim from upstream
sed -n "/^RUN <<'EOF'$/,/^EOF$/p" $TB/Dockerfile.rocm-7.14 > /tmp/rocm_block
# cmake flags verbatim (the `-D...` line of the upstream build RUN)
grep -o -E '\-D[A-Z_]+=?[A-Za-z0-9/._-]*' $TB/Dockerfile.rocm-7.14 | sort -u > /tmp/cmake_flags

cat > $OUT <<'DOCKERFILE'
# generated from this recipe; do not hand-edit the ROCm block above
DOCKERFILE
# (then append, in order: the ROCm block, the toolchain install, our stages below)
```

…or write the file by hand from this and diff it against upstream's before building:

```dockerfile
# ---------- builder ----------
FROM registry.fedoraproject.org/fedora:44 AS builder

# >>> paste the ROCm repo heredoc block VERBATIM from
#     amd-strix-halo-toolboxes/toolboxes/Dockerfile.rocm-7.14 <<<

# upstream's toolchain set + ccache (parity: same meta-package the CI uses)
RUN dnf -y --nodocs --setopt=install_weak_deps=False install \
      make gcc gcc-c++ cmake libcurl-devel ninja-build rdma-core-devel \
      amdrocm-core-devel7.14-gfx1151 \
      git-core ccache patch which \
 && dnf clean all && rm -rf /var/cache/dnf/*

ENV ROCM_PATH=/opt/rocm \
    HIP_PATH=/opt/rocm \
    PATH=/opt/rocm/bin:/opt/rocm/core/bin:/opt/rocm/core/lib/llvm/bin:$PATH \
    LD_LIBRARY_PATH=/opt/rocm/core/lib/rocm_sysdeps/lib:/opt/rocm/core/lib \
    CCACHE_DIR=/ccache CCACHE_MAXSIZE=5G

# source comes from the build context (the fork tree); patches from a named context.
# COPY only — bind mounts of host paths into RUN fail on this box (context-relative
# resolution, and SELinux denial for named-context binds). See HANDOFF §2.
WORKDIR /opt/llama-fork-src
COPY . /opt/llama-fork-src/
COPY --from=patches llama-grammar.patch /tmp/patches/
COPY --from=patches llama-cpp-25992-rocm-host-buffer.patch /tmp/patches/

# parity patches: BOTH are required to match production behaviour, and this fork
# contains neither (verified: src/llama-grammar.cpp MAX_REPETITION_THRESHOLD is 2000;
# the #25992 host-buffer symbols are absent). `best` = warn and continue,
# `strict` = fail the build, `none` = knowingly diverge.
ARG APPLY_PATCHES=best
RUN set -eu; cd /opt/llama-fork-src; for p in /tmp/patches/*.patch; do \
      if patch -p1 --dry-run --silent < "$p" 2>/dev/null; then \
        patch -p1 --silent < "$p"; echo "applied $(basename "$p")"; \
      elif patch -p1 --reverse --dry-run --silent < "$p" 2>/dev/null; then \
        echo "already present: $(basename "$p")"; \
      else \
        echo "PATCH DOES NOT APPLY: $(basename "$p")"; \
        [ "$APPLY_PATCHES" = strict ] && exit 1; \
        [ "$APPLY_PATCHES" = best ] && echo "  continuing WITHOUT production parity"; \
      fi; done

# >>> paste the cmake flags VERBATIM from upstream, then the deltas below <<<
#     (DGGML_HIP / DAMDGPU_TARGETS=gfx1151 / DCMAKE_BUILD_TYPE=Release /
#      DGGML_RPC / DROCM_PATH / DHIP_PLATFORM)
ARG CMAKE_BUILD_TYPE=Release
ARG BUILD_JOBS=0
RUN --mount=type=cache,target=/ccache set -eu; \
    cmake -S /opt/llama-fork-src -B /opt/llama-fork-build \
      -DCMAKE_BUILD_TYPE="${CMAKE_BUILD_TYPE}" \
      -DCMAKE_INSTALL_PREFIX=/opt/llama-fork \
      -DCMAKE_C_COMPILER_LAUNCHER=ccache \
      -DCMAKE_CXX_COMPILER_LAUNCHER=ccache \
      ${EXTRA_CMAKE_ARGS:-} \
 && cmake --build /opt/llama-fork-build -- ${BUILD_JOBS:+-j}${BUILD_JOBS:-$(nproc)} \
 && cmake --install /opt/llama-fork-build

# ---------- runtime ----------
ARG BASE_IMAGE=docker.io/kyuz0/amd-strix-halo-toolboxes:rocm-7.14
FROM ${BASE_IMAGE} AS runtime

COPY --from=builder /opt/llama-fork/ /opt/llama-fork/
RUN printf '%s\n%s\n' /opt/llama-fork/lib /opt/llama-fork/lib64 \
      > /etc/ld.so.conf.d/zz-llama-fork.conf && ldconfig

ARG GIT_REF=unknown GIT_SHA=unknown GIT_DIRTY=unknown BASE_IMAGE_TAG=unknown BUILD_DATE=unknown
RUN { echo "ref=${GIT_REF}"; echo "sha=${GIT_SHA}"; echo "dirty=${GIT_DIRTY}"; \
      echo "base=${BASE_IMAGE_TAG}"; echo "built=${BUILD_DATE}"; } > /opt/llama-fork/BUILDINFO

ENV PATH=/opt/llama-fork/bin:$PATH
CMD ["llama-server", "--version"]
```

Notes on the two deliberate deviations from upstream:

* **install prefix `/opt/llama-fork`, not `/usr/local`.** The stock binary stays reachable at
  `/usr/local/bin/llama-server`, so a single container can A/B patched vs production. It
  also means a broken build cannot make the image unusable.
* **ccache for host C/CXX only.** Our changes are host code; HIP translation units are the
  long pole and are not cached. Expect "edit host file → relink in seconds", not "full
  rebuild in seconds" (*expected, not yet measured*).

## Step 2 — build

```sh
cd ~/source/oLLM                       # build context = the fork tree
podman build \
  -f docs/development/disk-cache/container/Dockerfile.fork \
  --ignorefile docs/development/disk-cache/container/containerignore \
  --build-context patches=$HOME/source/setup/amd-strix-halo-toolboxes/toolboxes \
  --build-arg GIT_REF=$(git rev-parse --abbrev-ref HEAD) \
  --build-arg GIT_SHA=$(git rev-parse --short HEAD)$(git diff --quiet HEAD || echo -dirty) \
  --build-arg BASE_IMAGE_TAG=docker.io/kyuz0/amd-strix-halo-toolboxes:rocm-7.14 \
  -t localhost/llama-fork:disk-cache-$(git rev-parse --short HEAD) .
```

Cost, expected: ~1.5–2 GB of downloads for the builder stage (cached in podman's layer
store afterwards) and a 20–60 min HIP compile on 32 cores. Pass `--build-arg
BUILD_JOBS=6` if anything else is using memory.

**Stop the server first.** `systemctl --user stop llama-server` — Flash-Next keeps ~110 GiB
of 121 GiB resident and hipcc × 32 on top of that is an OOM/swap event, not a slow build.

## Step 3 — run against the real model, off production's port

```sh
systemctl --user is-active llama-server && echo "STOP THE SERVER FIRST (RAM)" 
podman run --rm --name llama-fork \
  --device /dev/dri --device /dev/kfd \
  --group-add video --group-add render --group-add sudo \
  --security-opt seccomp=unconfined --network host \
  --userns keep-id --user rain \
  -v /home/rain:/home/rain --workdir /home/rain \
  -e LD_LIBRARY_PATH=/opt/llama-fork/lib64:/opt/llama-fork/lib:/opt/rocm/core/lib/rocm_sysdeps/lib:/opt/rocm/core/lib \
  localhost/llama-fork:disk-cache-<sha> \
  llama-server --model /home/rain/models/qwen3.8-27b/Qwen3.8-27B-UD-Q4_K_XL.gguf \
    --alias qwen3.8-fork --host 0.0.0.0 --port 1246 \
    --ctx-size 32768 --parallel 1 --flash-attn on --load-mode none \
    --cache-type-k q8_0 --cache-type-v q8_0 \
    --cache-disk 307200 --cache-disk-dir /home/rain/.cache/llama-fork/
```

The device/group flags are copied verbatim from `refresh-toolboxes.sh` (`grep -o
-E '\-\-security-opt .*' ` there). `--userns keep-id` matters: without it the container
writes root-owned files into `~/.cache`.

Which build am I actually running (do not skip — this repo has been bitten twice by
"thought I was running the patched one"):

```sh
podman exec llama-fork bash -lc 'cat /opt/llama-fork/BUILDINFO; command -v llama-server; \
  ldd $(command -v llama-server) | grep -E "llama|ggml"; tr "\0" " " < /proc/$(pgrep -x llama-server | head -1)/cmdline'
```

## Step 4 — recovery (the point of the whole exercise)

```sh
podman stop llama-fork; podman rm llama-fork            # test container
podman rmi localhost/llama-fork:disk-cache-<sha>        # our images only
```

Nothing else was touched: `docker.io/kyuz0/...` images, the `llama-rocm-7.14` /
`llama-flash-next` toolboxes, the systemd units, and port 1245 are all unchanged. If you
want the *original* behaviour back mid-experiment: `systemctl --user start llama-server`.

## Flash-Next

Same Dockerfile with `--build-arg`-style changes to the source: `qwen4exp` is not in this
fork, so either (a) build a branch that merges `danielhanchen/llama.cpp`
`qwen4exp/qwen3.8-flash-next` (upstream PR #27793), or (b) keep the *source* as upstream and
only test on `qwen35`. Do not base the runtime stage on
`:rocm-7.14-qwen-3.8-flash-next` and expect our fork's libraries to load `qwen4exp` — the
architecture table comes from the compiled libraries, not the image.
