#!/bin/bash
cd /Users/sw/heart-portal
echo "Running cargo check..."
cargo check 2>&1
echo "Exit code: $?"