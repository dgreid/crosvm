#!/bin/bash
# Boot test script for macOS ARM64
#
# Usage:
#   ./tools/test_macos_boot.sh [kernel_image] [timeout_seconds]
#
# Environment variables:
#   CROSVM_BIN - Path to crosvm binary (default: target/release/crosvm)
#   CROSVM_ENTITLEMENTS - Path to entitlements file (default: crosvm.entitlements)

set -e

KERNEL="${1:-arm64_Image}"
TIMEOUT="${2:-30}"
CROSVM_BIN="${CROSVM_BIN:-target/release/crosvm}"
CROSVM_ENTITLEMENTS="${CROSVM_ENTITLEMENTS:-crosvm.entitlements}"
BOOT_LOG="/tmp/crosvm_boot_test.log"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "=== crosvm macOS Boot Test ==="
echo ""

# Check for kernel image
if [ ! -f "$KERNEL" ]; then
    echo -e "${RED}Error: Kernel image not found: $KERNEL${NC}"
    echo "Please provide a valid ARM64 Linux kernel image."
    echo "Usage: $0 [kernel_image] [timeout_seconds]"
    exit 1
fi

echo "Kernel: $KERNEL"
echo "Timeout: ${TIMEOUT}s"
echo "Binary: $CROSVM_BIN"
echo ""

# Build if binary doesn't exist
if [ ! -f "$CROSVM_BIN" ]; then
    echo "Building crosvm..."
    cargo build --release -p crosvm
fi

# Check if entitlements file exists
if [ ! -f "$CROSVM_ENTITLEMENTS" ]; then
    echo "Creating entitlements file..."
    cat > "$CROSVM_ENTITLEMENTS" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
</dict>
</plist>
EOF
fi

# Sign binary if not already signed with correct entitlements
echo "Checking code signature..."
if ! codesign -v "$CROSVM_BIN" 2>/dev/null; then
    echo "Signing crosvm binary with HVF entitlements..."
    codesign --sign - --entitlements "$CROSVM_ENTITLEMENTS" --force "$CROSVM_BIN"
fi

# Run boot test
echo ""
echo "Starting boot test..."
echo "Running: timeout $TIMEOUT $CROSVM_BIN run $KERNEL"
echo ""

# Run crosvm with timeout, capturing output
set +e
timeout "$TIMEOUT" "$CROSVM_BIN" run "$KERNEL" 2>&1 | tee "$BOOT_LOG"
EXIT_CODE=$?
set -e

echo ""
echo "=== Boot Test Results ==="
echo ""

# Check results
PASSED=0
FAILED=0

check_output() {
    local pattern="$1"
    local description="$2"
    if grep -q "$pattern" "$BOOT_LOG" 2>/dev/null; then
        echo -e "${GREEN}[PASS]${NC} $description"
        ((PASSED++))
        return 0
    else
        echo -e "${RED}[FAIL]${NC} $description"
        ((FAILED++))
        return 1
    fi
}

check_output_optional() {
    local pattern="$1"
    local description="$2"
    if grep -q "$pattern" "$BOOT_LOG" 2>/dev/null; then
        echo -e "${GREEN}[PASS]${NC} $description"
        ((PASSED++))
    else
        echo -e "${YELLOW}[SKIP]${NC} $description (optional)"
    fi
}

# Core boot checks
check_output "Booting Linux" "Kernel started booting"
check_output_optional "Linux version" "Kernel version printed"
check_output_optional "Machine model: crosvm" "Device tree loaded"
check_output_optional "psci:" "PSCI initialized"
check_output_optional "clocksource:" "Timer working"
check_output_optional "smp:" "SMP initialized"

echo ""
echo "Summary: $PASSED passed, $FAILED failed"
echo ""

# Show last lines of log if there were failures
if [ $FAILED -gt 0 ]; then
    echo "=== Last 20 lines of boot log ==="
    tail -20 "$BOOT_LOG"
    echo ""
fi

# Basic success: kernel must at least start booting
if grep -q "Booting Linux" "$BOOT_LOG" 2>/dev/null; then
    echo -e "${GREEN}Boot test PASSED${NC} - Kernel started successfully"
    exit 0
else
    echo -e "${RED}Boot test FAILED${NC} - Kernel did not start"
    echo ""
    echo "Full log available at: $BOOT_LOG"
    exit 1
fi
