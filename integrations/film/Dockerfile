# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The film for a Linux x86-64 machine with an NVIDIA card: the render on
# Vulkan, the film encoded by the card (NVENC, over CUDA).
#
# tuile-film-produce, the entry point, bakes, renders and joins a whole film
# kept in the run store; on a Cloud Run job of N tasks, each task takes its
# share of the packs and the last to finish joins. tuile-film-render is
# beside it, for one render by hand.
#
#   docker build -f integrations/film/Dockerfile -t tuile-film .
#   docker run --gpus all -e TUILE_STORE_ENDPOINT=… -e TUILE_STORE_BUCKET=… \
#     -e TUILE_STORE_ACCESS_KEY_ID=… -e TUILE_STORE_SECRET_ACCESS_KEY=… \
#     -e TUILE_TILES_BUCKET=… tuile-film store:tapes/flight.mcap --resolution 3840x2160
#
# The build has been through a compiler; nothing here has yet run on a card.

ARG CUDA=12.4.1
ARG UBUNTU=22.04
ARG NVIDIA_DRIVER=550

FROM nvidia/cuda:${CUDA}-devel-ubuntu${UBUNTU} AS build
ARG NVIDIA_DRIVER
# libnvidia-encode and libnvcuvid come with the driver, not with CUDA. The
# bindings' build wants both, side by side and under their unversioned
# names, which only the linker ever asks for and no package ships; at run
# time the host's own are mounted in their place. CUDA itself is not linked:
# it is opened at run time, and here only `nvcc` is asked its version.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates cmake curl nasm pkg-config \
        libnvidia-encode-${NVIDIA_DRIVER} libnvidia-decode-${NVIDIA_DRIVER} \
    && rm -rf /var/lib/apt/lists/* \
    && cd /usr/lib/x86_64-linux-gnu \
    && for lib in libnvidia-encode libnvcuvid; do \
        test -e $lib.so.1 && ln -sf $lib.so.1 $lib.so || exit 1; \
    done
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH CUDA_PATH=/usr/local/cuda
WORKDIR /src
COPY . .
# How many compilers at once: the builder's memory decides, not the recipe.
ARG FILM_BUILD_JOBS=4
# x86 has an assembler here, so rav1e's assembly comes along for the
# machines whose card encodes nothing. The registry and the build directory
# stay with the builder between two builds, so a correction costs the crates
# it touches and not the whole tree. The last line shows what the binary
# asks the machine for: the card's encoder must be in it.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -j ${FILM_BUILD_JOBS} \
        -p tuile-film-native --features nvenc,asm \
        --bin tuile-film-produce --bin tuile-film-render \
    && cp target/release/tuile-film-produce target/release/tuile-film-render /usr/local/bin/ \
    && ldd /usr/local/bin/tuile-film-produce \
    && for bin in tuile-film-produce tuile-film-render; do \
        ldd /usr/local/bin/$bin | grep -q libnvidia-encode || exit 1; \
    done

FROM nvidia/cuda:${CUDA}-runtime-ubuntu${UBUNTU}
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libvulkan1 libx11-6 libxext6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/tuile-film-produce /usr/local/bin/tuile-film-render /usr/local/bin/
# The Vulkan loader finds a driver by its manifest, and the host mounts the
# NVIDIA driver's libraries (on Cloud Run: /usr/local/nvidia/lib64, on the
# library path) without one. Measured on a Cloud Run L4, driver 580: no
# manifest, "vulkan drivers/libraries could not be loaded", and no adapter.
# The library is named, not pathed: the loader looks it up on the library path.
# libGLX_nvidia links X11 even with no display; without libX11 and libXext
# the loader skips the driver ("libX11.so.6: cannot open shared object file").
RUN mkdir -p /etc/vulkan/icd.d \
    && printf '%s\n' '{"file_format_version": "1.0.0", "ICD": {"library_path": "libGLX_nvidia.so.0", "api_version": "1.3.0"}}' \
        > /etc/vulkan/icd.d/nvidia_icd.json
# The card's Vulkan driver, its encoder and CUDA, all from the host.
# The render asks wgpu for Vulkan by name: a card that offers it is not
# left to pick another backend.
ENV NVIDIA_DRIVER_CAPABILITIES=all TUILE_CACHE_DIR=/cache WGPU_BACKEND=vulkan
VOLUME /cache
ENTRYPOINT ["tuile-film-produce"]
