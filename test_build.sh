#!/bin/bash

echo "Testing Heart Portal Phase 2 Extension Manager Build"
echo "===================================================="

cd portal

echo "1. Checking Rust code..."
if cargo check --quiet; then
    echo "   ✓ Rust code compiles successfully"
else
    echo "   ✗ Rust compilation failed"
    exit 1
fi

echo "2. Running tests..."
if cargo test --quiet; then
    echo "   ✓ All tests pass"
else
    echo "   ✗ Some tests failed"
    exit 1
fi

echo "3. Building release binary..."
if cargo build --release --quiet; then
    echo "   ✓ Release build successful"
else
    echo "   ✗ Release build failed"
    exit 1
fi

echo "4. Checking binary size..."
BINARY_SIZE=$(stat -f%z target/release/heart-portal 2>/dev/null || stat -c%s target/release/heart-portal 2>/dev/null || echo "unknown")
echo "   Binary size: $BINARY_SIZE bytes"

echo ""
echo "✓ Phase 2 Extension Manager build completed successfully!"
echo ""
echo "Features implemented:"
echo "  - Dual-layer architecture (kernel + extensions)"
echo "  - extensions.toml configuration system"
echo "  - Hot reload functionality"
echo "  - Extension management API (start/stop/restart/status/reload)"
echo "  - Integration with existing ToolHost"
echo ""
echo "To test:"
echo "  1. Start portal: ./target/release/heart-portal"
echo "  2. Run test script: python3 ../test_extensions.py"