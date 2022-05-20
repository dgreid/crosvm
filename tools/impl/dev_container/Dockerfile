# Copyright 2021 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Development container for crosvm.
#
# Provides all dependencies specified in install-deps with some additonal
# logic to cache cargo data in CI runs.
#
# Note, if you are using docker, you will probably be using "Dockerfile.user".

FROM docker.io/debian:testing-20220822-slim

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

# Use a dedicated target directory so we do not write into the source directory.
RUN mkdir -p /scratch/cargo_target \
    && mkdir /cache

# Prevent the container from writing __pycache__ files into the src.
ENV PYTHONDONTWRITEBYTECODE=1
ENV CARGO_TARGET_DIR=/scratch/cargo_target


# rust toolchain setup taken from rust crate
# duplicated here so riscv64 support can be used from debian unstable
ARG RUST_VERSION

ENV RUSTUP_HOME=/usr/local/rustup \
    RUST_ARCH=x86_64-unknown-linux-gnu \
    RUSTUP_VERSION=1.25.1 \
    CARGO_HOME=/usr/local/cargo \
    rust_version=$RUST_VERSION \
    PATH=/usr/local/cargo/bin:$PATH

RUN set -eux; \
    url="https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$RUST_ARCH/rustup-init"; \
    wget -q "$url"; \
    chmod +x rustup-init;
RUN ./rustup-init -y --no-modify-path --profile minimal --default-toolchain $RUST_VERSION --default-host $RUST_ARCH;
RUN rm rustup-init;
RUN chmod -R a+w $RUSTUP_HOME $CARGO_HOME;

# Allow APT to cache packages between docker image builds
RUN rm -f /etc/apt/apt.conf.d/docker-clean; echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache

# Setting this will help install-deps skip installing wine deps, which
# will be installed by Dockerfile.user
ENV DOCKER_WINE_SETUP=1

# Install dependencies (APT and cargo packages are cached between image builds for faster iterative builds).
COPY tools/install-deps tools/install-aarch64-deps tools/install-armhf-deps tools/install-riscv64-deps tools/install-docs-deps rust-toolchain /tools/
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    --mount=type=cache,target=/scratch/cargo_target,sharing=locked \
    cd /tools \
    && chmod +x * \
    && apt-get update \
    && apt-get install --yes sudo \
    && apt-get install --yes debian-ports-archive-keyring \
    && ./install-deps \
    && ./install-aarch64-deps \
    && ./install-armhf-deps \
    && ./install-riscv64-deps \
    && ./install-docs-deps

# Prepare wine64
RUN sudo ln -sf /usr/bin/wine64-stable /usr/bin/wine64 \
    && wine64 wineboot

# Prebuild aarch64 VM image for faster startup.
COPY tools/aarch64vm /tools/
COPY tools/riscv64vm /tools/
COPY /tools/impl/testvm.py /tools/impl/
COPY /tools/impl/testvm/version /tools/impl/testvm/
RUN sudo chmod 755 -R /tools \
    && /tools/aarch64vm build
    && /tools/riscv64vm build

# Cache CARGO_HOME between container runs in CI.
VOLUME /cache
ENV CARGO_HOME=/cache/cargo_home

VOLUME /workspace
WORKDIR /workspace
