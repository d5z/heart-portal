#!/usr/bin/env python3
import subprocess
import os
import sys

os.chdir('/Users/sw/heart-portal')

try:
    result = subprocess.run(['cargo', 'check'], capture_output=True, text=True, timeout=60)
    print("STDOUT:")
    print(result.stdout)
    print("\nSTDERR:")
    print(result.stderr)
    print(f"\nReturn code: {result.returncode}")
except subprocess.TimeoutExpired:
    print("Cargo check timed out")
except Exception as e:
    print(f"Error running cargo check: {e}")