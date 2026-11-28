#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds the CI images of integrations/docker/Dockerfile.ci on a remote
# BuildKit builder, never on this machine, and optionally pushes them:
#
#   tuile-ci-builder  Ubuntu 24.04 + everything: the base of the builder's test image
#   tuile-ci          the actions-runner image of the `tuile-linux` pod + the same layers
#
# One Dockerfile, one set of layers: the two differ by the image they start FROM.
#
#   TUILE_BUILDER=stl-builder-k8s scripts/build-ci-images.sh               # build only, nothing pushed
#   TUILE_BUILDER=stl-builder-k8s scripts/build-ci-images.sh runner   # one of the two
#   TUILE_CI_PUSH=1 GHCR_USER=<login> GHCR_TOKEN=<token> TUILE_BUILDER=… scripts/build-ci-images.sh
#
# Without TUILE_CI_PUSH nothing leaves the builder: the build runs (and warms
# the builder's cache) and the script prints each image's tag it would push.
# With it, each image is pushed as
#
#   ghcr.io/lapoule-dev/tuile-ci-<name>:<tag>      <tag> = rust<RUST>-nextest<NEXTEST>-<git sha>[-dirty]
#
# and its digest is printed (`name@sha256:…`): the value to pin in the runner's
# values and in Dockerfile.test's CI_BASE_IMAGE, never the tag.
#
# The registry credentials are read from the environment (GHCR_USER, and
# GHCR_TOKEN: a token with `write:packages`), written to a throwaway
# DOCKER_CONFIG that is removed at exit; they never appear on a command line.
#
# Versions: the defaults of Dockerfile.ci. To bump a tool, edit its ARG
# there (and its checksum), or override it for one run:
#   TUILE_CI_BUILD_ARGS="--build-arg RUST_VERSION=1.101.0 --build-arg RUSTUP_VERSION=…"
# The tag is derived from the Dockerfile's ARG values, so an override is in it.
#
#   TUILE_BUILDER       buildx builder to run on                      (always)
#   TUILE_CI_REGISTRY   default ghcr.io/lapoule-dev
#   TUILE_CI_RUNNER     the actions-runner image the pod image starts from
#                       (default: the one pinned in the infra's tuile-linux values)
#   TUILE_CI_PUSH       1 to push
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOCKERFILE="${REPO_ROOT}/integrations/docker/Dockerfile.ci"
CONTEXT="${REPO_ROOT}/integrations/docker"

case "${1:-}" in -h|--help) sed -n '5,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;; esac
if [ -z "${TUILE_BUILDER:-}" ]; then
    echo "error: TUILE_BUILDER is not set — the buildx builder to build on" >&2
    exit 2
fi

REGISTRY="${TUILE_CI_REGISTRY:-ghcr.io/lapoule-dev}"
RUNNER_IMAGE="${TUILE_CI_RUNNER:-ghcr.io/actions/actions-runner:2.338.2@sha256:6d0b2d80d80f143f77dcb676e50920100bbb547e39e883454d57f45ab8c31300}"
PUSH="${TUILE_CI_PUSH:-}"
IMAGES=("$@")
[ "${#IMAGES[@]}" -gt 0 ] || IMAGES=(builder runner)

# The default of an ARG of the Dockerfile, or the override given for this run.
arg() {
    local v
    v="$(printf '%s' " ${TUILE_CI_BUILD_ARGS:-}" | sed -nE "s/.*--build-arg ${1}=([^ ]+).*/\1/p" | tail -n1)"
    [ -n "$v" ] || v="$(sed -nE "s/^ARG ${1}=([^ ]+).*/\1/p" "$DOCKERFILE" | head -n1)"
    printf '%s' "$v"
}
SHA="$(git -C "$REPO_ROOT" rev-parse --short=9 HEAD)"
git -C "$REPO_ROOT" diff --quiet -- integrations/docker || SHA="${SHA}-dirty"
TAG="rust$(arg RUST_VERSION)-nextest$(arg NEXTEST_VERSION)-${SHA}"

DOCKER_CONFIG_DIR=""
if [ -n "$PUSH" ]; then
    if [ -z "${GHCR_USER:-}" ] || [ -z "${GHCR_TOKEN:-}" ]; then
        echo "error: TUILE_CI_PUSH=1 needs GHCR_USER and GHCR_TOKEN (a token with write:packages) in the environment" >&2
        exit 2
    fi
    DOCKER_CONFIG_DIR="$(mktemp -d)"
    trap 'rm -rf "$DOCKER_CONFIG_DIR"' EXIT
    # The plugins, the builders and the contexts stay where they are; only the credentials are throwaway.
    mkdir -p "$DOCKER_CONFIG_DIR"
    for d in cli-plugins buildx contexts; do
        ln -s "${DOCKER_CONFIG:-$HOME/.docker}/${d}" "$DOCKER_CONFIG_DIR/${d}" 2>/dev/null || true
    done
    printf '%s' "$GHCR_TOKEN" | DOCKER_CONFIG="$DOCKER_CONFIG_DIR" docker login ghcr.io -u "$GHCR_USER" --password-stdin > /dev/null
fi

build() { # $1=name, $2=image name, then the extra build args
    local name="$1" image="$2" ref meta start
    shift 2
    ref="${REGISTRY}/${image}:${TAG}"
    meta="$(mktemp)"
    start=$(date +%s)
    echo "=== ${name}: ${ref} on ${TUILE_BUILDER}"
    local out=(--output "type=cacheonly")
    [ -z "$PUSH" ] || out=(--output "type=image,name=${ref},push=true")
    # shellcheck disable=SC2086
    DOCKER_CONFIG="${DOCKER_CONFIG_DIR:-${DOCKER_CONFIG:-$HOME/.docker}}" \
        docker buildx build --builder "$TUILE_BUILDER" --platform linux/amd64 --progress plain \
        -f "$DOCKERFILE" --metadata-file "$meta" \
        --build-arg "IMAGE_VARIANT=${name}" ${TUILE_CI_BUILD_ARGS:-} "$@" \
        "${out[@]}" "$CONTEXT"
    echo "=== ${name}: built in $(( $(date +%s) - start )) s"
    if [ -n "$PUSH" ]; then
        local digest
        digest="$(sed -nE 's/.*"containerimage.digest": *"([^"]+)".*/\1/p' "$meta")"
        echo "=== ${name}: pushed ${REGISTRY}/${image}@${digest}"
    else
        echo "=== ${name}: not pushed (TUILE_CI_PUSH unset); would be ${ref}"
    fi
    rm -f "$meta"
}

# One heavy build at a time: the builder is shared; the second build reuses
# every layer the two have in common.
for name in "${IMAGES[@]}"; do
    case "$name" in
        builder) build builder tuile-ci-builder ;;
        runner)  build runner tuile-ci --build-arg "BASE_IMAGE=${RUNNER_IMAGE}" --build-arg CI_USER=runner ;;
        *) echo "error: unknown image '${name}' (builder, runner)" >&2; exit 2 ;;
    esac
done
