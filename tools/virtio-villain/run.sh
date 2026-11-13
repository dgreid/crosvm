#!/usr/bin/env bash
# Copyright 2026 The ChromiumOS Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Run the virtio-villain hostile guest test suite against crosvm.
#
# virtio-villain (https://github.com/weltling/virtio-villain) boots a
# minimal initramfs guest that drives virtio devices directly from
# userspace and violates the spec on purpose. It does not ship a crosvm
# backend yet, so this script applies crosvm-backend.patch to a
# virtio-villain checkout before running it.
#
# See README.md in this directory for details and TODO.md for the
# issues found.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CROSVM_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PATCH="${SCRIPT_DIR}/crosvm-backend.patch"

# virtio-villain commit the patch applies to and the results in TODO.md
# were produced with.
VILLAIN_URL="https://github.com/weltling/virtio-villain"
VILLAIN_REV="94b85dd1abdced6cc76e33c0428db1d4d10786d1"

VILLAIN_DIR=""
LINUX_SRC=""
KERNEL=""
CROSVM=""
SANDBOX=0
NET=1
JOBS=32
RETRIES=2
TIMEOUT=30
OUT="${CROSVM_ROOT}/.tmp/virtio-villain"
TESTS=()

usage() {
    cat <<EOF
usage: $0 [options] [TEST ...]

Runs the whole suite when no TEST IDs are given, e.g. "$0 P0021 T0048".

options:
  --villain DIR   virtio-villain checkout (default: clone into OUT/virtio-villain)
  --linux DIR     Linux git tree to build the guest kernel from (a clean
                  worktree is made, DIR itself is not touched)
  --kernel PATH   prebuilt guest kernel (bzImage). Without this or --linux
                  virtio-villain downloads its default kernel from GitHub
  --crosvm PATH   crosvm binary (default: cargo build --release here)
  --sandbox       run crosvm with its default sandbox instead of
                  --disable-sandbox
  --no-net        don't create tap devices; net tests will SKIP
  --jobs N        parallel VMs (default: ${JOBS})
  --retries N     retry failing tests N times (default: ${RETRIES})
  --timeout SEC   per test timeout (default: ${TIMEOUT})
  --out DIR       results directory (default: ${OUT})

Net tests need tap devices, which needs sudo. Without --sandbox the taps
are multiqueue, and crosvm needs CAP_NET_ADMIN to open more than one
queue (see TODO.md), so a copy of the crosvm binary is given
cap_net_admin with setcap. Both are removed on exit.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    --villain) VILLAIN_DIR="$2"; shift 2 ;;
    --linux) LINUX_SRC="$2"; shift 2 ;;
    --kernel) KERNEL="$2"; shift 2 ;;
    --crosvm) CROSVM="$2"; shift 2 ;;
    --sandbox) SANDBOX=1; shift ;;
    --no-net) NET=0; shift ;;
    --jobs) JOBS="$2"; shift 2 ;;
    --retries) RETRIES="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    -*) echo "unknown option $1" >&2; usage >&2; exit 2 ;;
    *) TESTS+=("$1"); shift ;;
    esac
done

mkdir -p "${OUT}"
OUT="$(cd "${OUT}" && pwd)"

TAPS=()
CAP_BINARY=""
WORKTREE=""
# shellcheck disable=SC2317 # Invoked by the EXIT trap.
cleanup() {
    for tap in "${TAPS[@]}"; do
        sudo ip link delete "${tap}" 2>/dev/null || true
    done
    if [[ -n "${CAP_BINARY}" ]]; then
        rm -f "${CAP_BINARY}"
    fi
    if [[ -n "${WORKTREE}" ]]; then
        git -C "${LINUX_SRC}" worktree remove --force "${WORKTREE}" || true
    fi
}
trap cleanup EXIT

log() {
    echo "==> $*" >&2
}

# virtio-villain with the crosvm backend.
if [[ -z "${VILLAIN_DIR}" ]]; then
    VILLAIN_DIR="${OUT}/virtio-villain"
    if [[ ! -d "${VILLAIN_DIR}/.git" ]]; then
        log "cloning ${VILLAIN_URL}"
        git clone -q "${VILLAIN_URL}" "${VILLAIN_DIR}"
        git -C "${VILLAIN_DIR}" checkout -q "${VILLAIN_REV}"
    fi
fi
if git -C "${VILLAIN_DIR}" apply --reverse --check "${PATCH}" 2>/dev/null; then
    log "crosvm backend already applied to ${VILLAIN_DIR}"
else
    log "applying crosvm backend to ${VILLAIN_DIR}"
    git -C "${VILLAIN_DIR}" apply "${PATCH}"
fi

# The guest init. Recent GCC reports a false maybe-uninitialized in the
# plan9 tests, which -Werror turns into a build failure.
log "building virtio-villain initramfs"
make -s -C "${VILLAIN_DIR}" -j"$(nproc)" initramfs \
    CFLAGS="-static -O2 -Wall -Wextra -Werror -Wno-error=maybe-uninitialized" \
    >/dev/null

# Guest kernel, built with virtio-villain's bundled config.
if [[ -z "${KERNEL}" && -n "${LINUX_SRC}" ]]; then
    rev="$(git -C "${LINUX_SRC}" rev-parse HEAD)"
    KERNEL="${OUT}/kernel/images/bzImage-x86_64"
    if [[ ! -f "${KERNEL}" || "$(cat "${OUT}/kernel/rev" 2>/dev/null)" != "${rev}" ]]; then
        WORKTREE="$(mktemp -d "${OUT}/linux.XXXXXX")"
        log "building guest kernel from ${LINUX_SRC} at ${rev}"
        git -C "${LINUX_SRC}" worktree add -q --detach "${WORKTREE}" "${rev}"
        rm -rf "${OUT}/kernel"
        (cd "${VILLAIN_DIR}" && ./build-kernel --source "${WORKTREE}" --offline \
            --arch x86_64 --jobs "$(nproc)" \
            --config kernel/common.config --config kernel/x86_64.config \
            --output "${OUT}/kernel") >"${OUT}/kernel-build.log" 2>&1 ||
            { echo "kernel build failed, see ${OUT}/kernel-build.log" >&2; exit 1; }
        echo "${rev}" >"${OUT}/kernel/rev"
    fi
fi

if [[ -z "${CROSVM}" ]]; then
    log "building crosvm"
    cargo build --release --manifest-path "${CROSVM_ROOT}/Cargo.toml" >&2
    CROSVM="${CROSVM_ROOT}/target/release/crosvm"
fi

RUN_ARGS=()
unset VV_CROSVM_SANDBOX VV_CROSVM_TAPS
if [[ ${SANDBOX} -eq 1 ]]; then
    export VV_CROSVM_SANDBOX=1
fi

if [[ ${NET} -eq 1 ]]; then
    # One tap per runner thread, plus a few for retries on the main thread.
    count=$((JOBS + 4))
    log "creating ${count} tap devices (sudo)"
    for i in $(seq 0 $((count - 1))); do
        tap="vv$$t${i}"
        if [[ ${SANDBOX} -eq 1 ]]; then
            sudo ip tuntap add "${tap}" mode tap user "$(id -u)"
        else
            sudo ip tuntap add "${tap}" mode tap multi_queue user "$(id -u)"
        fi
        TAPS+=("${tap}")
        sudo ip link set "${tap}" up
    done
    VV_CROSVM_TAPS="$(IFS=,; echo "${TAPS[*]}")"
    export VV_CROSVM_TAPS
    if [[ ${SANDBOX} -eq 1 ]]; then
        # A single queue pair opens without CAP_NET_ADMIN.
        RUN_ARGS+=(--net-queues 1)
    else
        # The name must contain "crosvm" for virtio-villain to pick the
        # backend.
        CAP_BINARY="${OUT}/crosvm-net-admin.$$"
        cp "${CROSVM}" "${CAP_BINARY}"
        sudo setcap cap_net_admin+ep "${CAP_BINARY}"
        CROSVM="${CAP_BINARY}"
    fi
fi

if [[ -n "${KERNEL}" ]]; then
    RUN_ARGS+=(-k "${KERNEL}")
fi

mode=$([[ ${SANDBOX} -eq 1 ]] && echo sandbox || echo nosandbox)
RESULTS="${OUT}/results-${mode}"
rm -rf "${RESULTS}"
mkdir -p "${RESULTS}"

log "running virtio-villain, results in ${RESULTS}"
status=0
(cd "${VILLAIN_DIR}" && ./run -m "${CROSVM}" "${RUN_ARGS[@]}" \
    -j "${JOBS}" --retries "${RETRIES}" -t "${TIMEOUT}" \
    -l "${RESULTS}/logs" --format json --output "${RESULTS}/report.json" \
    "${TESTS[@]}") | tee "${RESULTS}/summary.txt" || status=$?

echo
echo "report:   ${RESULTS}/report.json"
echo "logs:     ${RESULTS}/logs/<TEST>.log"
exit "${status}"
