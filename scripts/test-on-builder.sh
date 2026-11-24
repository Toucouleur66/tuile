#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Runs the test suite on a remote BuildKit builder, not on this machine: the
# Linux jobs of .github/workflows/ci.yml, on their stack (Ubuntu 24.04,
# lavapipe for the GPU tests, TUILE_REQUIRE_GPU=1), as one image build whose
# output is the logs (integrations/docker/Dockerfile.test).
#
#   scripts/test-on-builder.sh                        # everything: test doc budget clippy wasm
#   scripts/test-on-builder.sh -p tuile-budget --lib  # `cargo nextest run` with these arguments
#   scripts/test-on-builder.sh -p tuile-wgpu -E 'test(no_black_ground)'
#   TUILE_TEST_STEPS="clippy wasm" scripts/test-on-builder.sh
#
# Arguments are `cargo nextest run`'s (after `--profile ci`); with arguments
# only the `test` step runs, unless TUILE_TEST_STEPS says otherwise. Each one
# reaches the image as it was given here (quoted with printf %q, parsed again
# by the step's shell): a filter expression with spaces survives.
#
# What is tested: the working tree's tracked files (committed or not;
# untracked files are not included), or TUILE_TEST_REF=<commit> if set.
#
# The logs land in target/test-on-builder/<time>/: <step>.log and <step>.exit
# per step, junit.xml, junit-budget.xml, summary.txt, exit-code and build.log.
# The script exits with the steps' code (the first failing one's).
#
# # Configuration
#
# Nothing about a deployment is baked in; it all comes from the environment.
#
#   TUILE_BUILDER          buildx builder to run on                  (always)
#   TUILE_TEST_STEPS       the steps, space-separated (default: all of them
#                          without arguments, `test` with arguments)
#   TUILE_TEST_REF         the commit to test instead of the working tree
#   TUILE_CARGO_JOBS       CARGO_BUILD_JOBS on the builder (default 4)
#   TUILE_TEST_THREADS     nextest --test-threads (default 4)
#
# The shared compilation cache, all optional (integrations/docker/sccache-env.sh);
# with no bucket or no key the steps compile without it:
#
#   TUILE_SCCACHE_BUCKET    S3 bucket of the sccache cache
#   TUILE_SCCACHE_REGION    its region
#   TUILE_SCCACHE_ENDPOINT  an S3-compatible endpoint, if not AWS
#   TUILE_SCCACHE_PREFIX    key prefix (default <arch>-gnu)
#   TUILE_SCCACHE_KEY_ID    access key id     } handed to the build as BuildKit
#   TUILE_SCCACHE_SECRET    secret access key } secrets, never as build args
#
# A deployment that shares its builders between several runs wraps this
# script in its own scheduling (one run per builder, a node's load checked
# before starting): it sets TUILE_BUILDER to the builder it was given.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

case "${1:-}" in -h|--help) sed -n '5,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;; esac
if [ -z "${TUILE_BUILDER:-}" ]; then
    echo "error: TUILE_BUILDER is not set — the buildx builder to run the tests on" >&2
    exit 2
fi

if [ "$#" -gt 0 ]; then
    STEPS="${TUILE_TEST_STEPS:-test}"
    TEST_ARGS="$(printf '%q ' "$@")"
else
    STEPS="${TUILE_TEST_STEPS:-test doc budget clippy wasm}"
    TEST_ARGS="--workspace"
fi

# A commit of the working tree as it is (tracked files), without touching it.
REF="${TUILE_TEST_REF:-$(git -C "$REPO_ROOT" stash create 2>/dev/null || true)}"
REF="${REF:-HEAD}"
COMMIT="$(git -C "$REPO_ROOT" rev-parse --short "$REF")"

OUT="${REPO_ROOT}/target/test-on-builder/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
CTX="${OUT}/context.tar"
git -C "$REPO_ROOT" archive --format=tar -o "$CTX" "$REF"

BUILD_ARGS=(
    --build-arg "CARGO_TEST_ARGS=${TEST_ARGS}"
    --build-arg "TEST_STEPS=${STEPS}"
    --build-arg "CARGO_JOBS=${TUILE_CARGO_JOBS:-4}"
    --build-arg "TEST_THREADS=${TUILE_TEST_THREADS:-4}"
)
# The compilation cache: its bucket as build args, its key as secrets read by
# buildx from this environment (the value never appears on a command line).
if [ -n "${TUILE_SCCACHE_BUCKET:-}" ] && [ -n "${TUILE_SCCACHE_KEY_ID:-}" ] \
    && [ -n "${TUILE_SCCACHE_SECRET:-}" ]; then
    export TUILE_SCCACHE_KEY_ID TUILE_SCCACHE_SECRET
    BUILD_ARGS+=(--build-arg "SCCACHE_BUCKET=${TUILE_SCCACHE_BUCKET}"
        --secret "id=sccache_key_id,env=TUILE_SCCACHE_KEY_ID"
        --secret "id=sccache_secret,env=TUILE_SCCACHE_SECRET")
    [ -n "${TUILE_SCCACHE_REGION:-}" ] && BUILD_ARGS+=(--build-arg "SCCACHE_REGION=${TUILE_SCCACHE_REGION}")
    [ -n "${TUILE_SCCACHE_ENDPOINT:-}" ] && BUILD_ARGS+=(--build-arg "SCCACHE_ENDPOINT=${TUILE_SCCACHE_ENDPOINT}")
    [ -n "${TUILE_SCCACHE_PREFIX:-}" ] && BUILD_ARGS+=(--build-arg "SCCACHE_PREFIX=${TUILE_SCCACHE_PREFIX}")
    echo "   sccache: s3://${TUILE_SCCACHE_BUCKET}/${TUILE_SCCACHE_PREFIX:-}"
else
    echo "   sccache: off (TUILE_SCCACHE_BUCKET / _KEY_ID / _SECRET not all set)"
fi

echo "=== ${STEPS} (cargo nextest run ${TEST_ARGS}) on ${TUILE_BUILDER} (${COMMIT}) → ${OUT}"
# Only what buildx prints of the steps: the "===" lines and the summaries.
# No pipefail for this one pipeline: PIPESTATUS keeps buildx's own status.
set +o pipefail
docker buildx build --builder "$TUILE_BUILDER" --platform linux/amd64 --progress plain \
    -f integrations/docker/Dockerfile.test \
    "${BUILD_ARGS[@]}" \
    --output "type=local,dest=${OUT}" \
    - < "$CTX" 2>&1 | tee "${OUT}/build.log" \
    | { grep --line-buffered -E '^#[0-9]+ [0-9.]+ (===|\[[a-z]+\] )' || true; }
build_rc="${PIPESTATUS[0]}"
set -o pipefail
rm -f "$CTX"

if [ "$build_rc" -ne 0 ] || [ ! -f "${OUT}/exit-code" ]; then
    echo "test-on-builder: the build itself failed (${OUT}/build.log)" >&2
    grep -E 'ERROR|error:' "${OUT}/build.log" | sed -n 1,20p >&2 || true
    exit 1
fi
rc="$(cat "${OUT}/exit-code")"
echo "=== exit ${rc}; logs in ${OUT}"
exit "$rc"
