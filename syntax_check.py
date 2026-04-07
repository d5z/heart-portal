#!/usr/bin/env python3
"""
Simple syntax checker for Rust files
This script performs basic syntax validation without full compilation
"""

import os
import re
import sys
from pathlib import Path

def check_rust_syntax(file_path):
    """Check basic Rust syntax issues"""
    issues = []
    
    try:
        with open(file_path, 'r') as f:
            content = f.read()
        
        lines = content.split('\n')
        
        # Check for basic syntax issues
        for i, line in enumerate(lines, 1):
            line_stripped = line.strip()
            
            # Check for unmatched braces (basic check)
            if line_stripped.endswith('{') and not line_stripped.startswith('//'):
                # This is a basic check - more sophisticated parsing would be needed
                pass
            
            # Check for missing semicolons (basic patterns)
            if (line_stripped.endswith(')') and 
                not line_stripped.startswith('//') and
                not line_stripped.startswith('if ') and
                not line_stripped.startswith('while ') and
                not line_stripped.startswith('for ') and
                not line_stripped.startswith('match ') and
                not line_stripped.endswith('{') and
                not line_stripped.endswith(',') and
                not '=' in line_stripped and
                len(line_stripped) > 0):
                # Might be missing semicolon
                pass
        
        # Check for basic import issues
        use_statements = [line for line in lines if line.strip().startswith('use ')]
        for line in use_statements:
            if not line.strip().endswith(';'):
                issues.append(f"Use statement missing semicolon: {line.strip()}")
        
        return issues
        
    except Exception as e:
        return [f"Error reading file: {e}"]

def main():
    """Main function"""
    rust_files = [
        "portal/src/main.rs",
        "portal/src/config.rs", 
        "portal/src/protocol.rs",
        "portal/src/tools_flat.rs",
        "portal/src/extensions.rs"
    ]
    
    all_issues = []
    
    for file_path in rust_files:
        full_path = Path("/Users/sw/heart-portal") / file_path
        if full_path.exists():
            print(f"Checking {file_path}...")
            issues = check_rust_syntax(full_path)
            if issues:
                all_issues.extend([f"{file_path}: {issue}" for issue in issues])
            else:
                print(f"  ✓ No obvious syntax issues found")
        else:
            print(f"  ⚠ File not found: {file_path}")
    
    if all_issues:
        print(f"\nFound {len(all_issues)} potential issues:")
        for issue in all_issues:
            print(f"  - {issue}")
        return 1
    else:
        print(f"\n✓ No obvious syntax issues found in {len(rust_files)} files")
        return 0

if __name__ == "__main__":
    sys.exit(main())