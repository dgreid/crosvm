# Copyright 2021 The Chromium OS Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
ARG RUST_VERSION
FROM ubuntu:22.04

# Use a dedicated target directory so we do not write into the source directory.
RUN mkdir -p /scratch/cargo_target
ENV CARGO_TARGET_DIR=/scratch/cargo_target

# Prevent the container from writing root-owned __pycache__ files into the src.
ENV PYTHONDONTWRITEBYTECODE=1

# Install dependencies for native builds.
COPY tools/install-deps tools/install-docs-deps /tools/
RUN apt-get update
RUN apt-get install --yes sudo
RUN apt-get install --yes debian-ports-archive-keyring
RUN /tools/install-deps
# Clear apt cache to save space in layer.
RUN rm -rf /var/lib/apt/lists/*
# Delete build artifacts from 'cargo install' to save space in layer.
RUN rm -rf /scratch/cargo_target/*

# Prepare wine64
RUN ln -sf /usr/bin/wine64-stable /usr/bin/wine64 \
    && wine64 wineboot

# Install cross-compilation dependencies.
COPY tools/install-aarch64-deps tools/install-armhf-deps tools/install-riscv64-deps /tools/

# Add foreign architectures for cross-compilation.
COPY tools/riscv64.list /etc/apt/sources.list.d/
COPY tools/arm64.list /etc/apt/sources.list.d/
COPY tools/armhf.list /etc/apt/sources.list.d/
RUN dpkg --add-architecture arm64 \
    && dpkg --add-architecture armhf \
    && dpkg --add-architecture riscv64
RUN apt-get update
RUN /tools/install-aarch64-deps
RUN /tools/install-armhf-deps
RUN /tools/install-riscv64-deps
# Clear apt cache to save space in layer.
RUN rm -rf /var/lib/apt/lists/*

# Prebuild aarch64 VM image for faster startup.
COPY tools/aarch64vm /tools/
COPY tools/riscv64vm /tools/
COPY /tools/impl/testvm.py /tools/impl/
COPY /tools/impl/testvm/version /tools/impl/testvm/
RUN /tools/aarch64vm build
RUN /tools/riscv64vm build

VOLUME /workspace
WORKDIR /workspace
