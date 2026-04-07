#!/usr/bin/env python3
"""
Test script for Heart Portal extension management
"""

import json
import socket
import time
import sys

def send_jsonrpc_request(host, port, method, params=None):
    """Send a JSON-RPC request to the portal"""
    request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params or {}
    }
    
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect((host, port))
        
        # Send request
        request_json = json.dumps(request) + "\n"
        sock.send(request_json.encode())
        
        # Receive response
        response_data = sock.recv(4096).decode().strip()
        sock.close()
        
        return json.loads(response_data)
    except Exception as e:
        print(f"Error: {e}")
        return None

def test_extensions():
    """Test extension management functionality"""
    host = "localhost"
    port = 9100
    
    print("Testing Heart Portal Extension Management")
    print("=" * 50)
    
    # Test 1: Initialize connection
    print("1. Testing initialize...")
    response = send_jsonrpc_request(host, port, "initialize", {
        "protocolVersion": "2024-11-05",
        "clientInfo": {"name": "test-client", "version": "1.0.0"}
    })
    if response and not response.get("error"):
        print("   ✓ Initialize successful")
    else:
        print(f"   ✗ Initialize failed: {response}")
        return
    
    # Test 2: Check extension status
    print("2. Testing extension status...")
    response = send_jsonrpc_request(host, port, "extensions/status")
    if response and not response.get("error"):
        extensions = response.get("result", {}).get("extensions", {})
        print(f"   ✓ Found {len(extensions)} extensions")
        for name, (status, error) in extensions.items():
            print(f"     - {name}: {status}" + (f" (error: {error})" if error else ""))
    else:
        print(f"   ✗ Status check failed: {response}")
    
    # Test 3: Reload extensions
    print("3. Testing extension reload...")
    response = send_jsonrpc_request(host, port, "extensions/reload")
    if response and not response.get("error"):
        changes = response.get("result", {}).get("changes", [])
        print(f"   ✓ Reload successful with {len(changes)} changes")
        for change in changes:
            print(f"     - {change}")
    else:
        print(f"   ✗ Reload failed: {response}")
    
    # Test 4: List tools (should include extension tools)
    print("4. Testing tools list...")
    response = send_jsonrpc_request(host, port, "tools/list")
    if response and not response.get("error"):
        tools = response.get("result", {}).get("tools", [])
        print(f"   ✓ Found {len(tools)} tools")
        for tool in tools:
            print(f"     - {tool['name']}: {tool['description']}")
    else:
        print(f"   ✗ Tools list failed: {response}")
    
    print("\nTest completed!")

if __name__ == "__main__":
    test_extensions()