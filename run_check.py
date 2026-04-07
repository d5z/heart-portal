#!/usr/bin/env python3
import subprocess
import os
import sys

os.chdir('/Users/sw/heart-portal')
try:
    result = subprocess.run(['cargo', 'check'], capture_output=True, text=True, timeout=60)
    print("STDOUT:")
    print(result.stdout)
    print("STDERR:")
    print(result.stderr)
    print(f"Return code: {result.returncode}")
except subprocess.TimeoutExpired:
    print("Command timed out after 60 seconds")
except Exception as e:
    print(f"Error: {e}")