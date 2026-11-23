# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# The bench on an x86-64 builder: put the children in ./tiles
# (fetch_children.py), then
#   docker buildx build --platform linux/amd64 --output type=local,dest=out .
# and read out/result.txt.
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake nasm && rm -rf /var/lib/apt/lists/*
ENV CARGO_BUILD_JOBS=2
WORKDIR /w
COPY . bench
RUN cd bench && cargo build --release --locked && cp target/release/derived-codecs /w/base \
 && cargo build --release --locked --features sse41 && cp target/release/derived-codecs /w/sse41 \
 && cargo build --release --locked --features avx2 && cp target/release/derived-codecs /w/avx2 \
 && rm -rf /w/bench/target
ARG RUN_ID=1
RUN (grep -m1 "model name" /proc/cpuinfo; grep -m1 -o -w "avx2\|sse4_1" /proc/cpuinfo | sort -u | tr '\n' ' '; nproc; cat /proc/loadavg; \
     for b in base sse41 avx2; do echo "### $b"; ./$b bench/tiles 60; done) > /w/result.txt 2>&1; cat /w/result.txt
FROM scratch
COPY --from=build /w/result.txt /
