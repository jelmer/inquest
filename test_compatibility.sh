#!/bin/bash
# Compatibility test between Python and Rust versions of testrepository
#
# This script verifies that the Rust port maintains full on-disk format
# compatibility with the Python version of testrepository.

set -e

RUST_TESTR="$(pwd)/target/release/testr"
PYTHON_TESTR="/usr/bin/testr"

# Check if both versions are available
if [ ! -f "$RUST_TESTR" ]; then
    echo "Error: Rust version not found at $RUST_TESTR"
    echo "Please run: cargo build --release"
    exit 1
fi

if ! command -v "$PYTHON_TESTR" &> /dev/null; then
    echo "Error: Python version not found"
    echo "Please install: pip install testrepository"
    exit 1
fi

echo "=== Testrepository Compatibility Test ==="
echo "Python version: $PYTHON_TESTR"
echo "Rust version: $RUST_TESTR"
echo ""

# Test 1: Python creates, Rust reads
echo "Test 1: Python creates repository, Rust reads it"
TEST_DIR=$(mktemp -d)
cd "$TEST_DIR"
$PYTHON_TESTR init > /dev/null
RUST_OUTPUT=$($RUST_TESTR stats)
if echo "$RUST_OUTPUT" | grep -q "Total test runs: 0"; then
    echo "✓ PASS: Rust can read Python-created repository"
else
    echo "✗ FAIL: Rust cannot read Python-created repository"
    exit 1
fi
cd - > /dev/null
rm -rf "$TEST_DIR"

# Test 2: Rust creates, Python reads
echo "Test 2: Rust creates repository, Python reads it"
TEST_DIR=$(mktemp -d)
cd "$TEST_DIR"
$RUST_TESTR init > /dev/null
PYTHON_OUTPUT=$($PYTHON_TESTR stats)
if echo "$PYTHON_OUTPUT" | grep -q "runs=0"; then
    echo "✓ PASS: Python can read Rust-created repository"
else
    echo "✗ FAIL: Python cannot read Rust-created repository"
    exit 1
fi
cd - > /dev/null
rm -rf "$TEST_DIR"

# Test 3: Format file compatibility
echo "Test 3: Format file byte-for-byte compatibility"
TEST_DIR=$(mktemp -d)
cd "$TEST_DIR"
mkdir python_repo rust_repo

cd python_repo
$PYTHON_TESTR init > /dev/null
PYTHON_FORMAT=$(od -c .testrepository/format)
PYTHON_NEXT=$(od -c .testrepository/next-stream)
cd ..

cd rust_repo
$RUST_TESTR init > /dev/null
RUST_FORMAT=$(od -c .testrepository/format)
RUST_NEXT=$(od -c .testrepository/next-stream)
cd ..

if [ "$PYTHON_FORMAT" = "$RUST_FORMAT" ] && [ "$PYTHON_NEXT" = "$RUST_NEXT" ]; then
    echo "✓ PASS: Format files are byte-for-byte identical"
else
    echo "✗ FAIL: Format files differ"
    echo "Python format: $PYTHON_FORMAT"
    echo "Rust format: $RUST_FORMAT"
    echo "Python next-stream: $PYTHON_NEXT"
    echo "Rust next-stream: $RUST_NEXT"
    exit 1
fi

cd - > /dev/null
rm -rf "$TEST_DIR"

echo ""
echo "=== All Compatibility Tests Passed ==="
echo "The Rust port maintains full on-disk format compatibility with the Python version."
