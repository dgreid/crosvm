#!/bin/bash
# Build and sign crosvm for macOS with HVF entitlements

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CROSVM_ROOT="$(dirname "$SCRIPT_DIR")"
ENTITLEMENTS="$CROSVM_ROOT/crosvm.entitlements"

# Parse arguments
BUILD_TYPE="release"
EXTRA_ARGS=()

for arg in "$@"; do
    case "$arg" in
        --debug)
            BUILD_TYPE="debug"
            ;;
        *)
            EXTRA_ARGS+=("$arg")
            ;;
    esac
done

if [[ "$BUILD_TYPE" == "release" ]]; then
    CARGO_ARGS="--release"
    TARGET_DIR="$CROSVM_ROOT/target/release"
else
    CARGO_ARGS=""
    TARGET_DIR="$CROSVM_ROOT/target/debug"
fi

echo "Building crosvm ($BUILD_TYPE)..."
cargo build $CARGO_ARGS -p crosvm "${EXTRA_ARGS[@]}"

echo "Signing with HVF entitlements..."
codesign --sign - --entitlements "$ENTITLEMENTS" --force "$TARGET_DIR/crosvm"

echo "Done: $TARGET_DIR/crosvm"
