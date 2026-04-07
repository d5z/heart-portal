#!/usr/bin/env python3
import subprocess
import os
import sys

def run_cargo_check():
    os.chdir('/Users/sw/heart-portal')
    
    try:
        # Run cargo check with json output to get structured error info
        result = subprocess.run(
            ['cargo', 'check', '--message-format=json'],
            capture_output=True,
            text=True,
            timeout=120
        )
        
        print("=== CARGO CHECK OUTPUT ===")
        print(f"Exit code: {result.returncode}")
        print("\n=== STDOUT ===")
        print(result.stdout)
        print("\n=== STDERR ===") 
        print(result.stderr)
        
        return result.returncode == 0
        
    except subprocess.TimeoutExpired:
        print("Cargo check timed out after 120 seconds")
        return False
    except Exception as e:
        print(f"Error running cargo check: {e}")
        return False

if __name__ == "__main__":
    success = run_cargo_check()
    sys.exit(0 if success else 1)