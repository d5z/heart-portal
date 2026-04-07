#!/bin/bash
cd /Users/sw/heart-portal
echo "Starting compilation test..."
timeout 30s cargo check --message-format=short 2>&1 | tee check_output.txt
echo "Exit code: $?"
echo "Output saved to check_output.txt"