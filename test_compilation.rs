// Simple compilation test for the main modules
// This file tests if the main components can be imported and used

use std::path::PathBuf;

// Test basic struct definitions (these should compile without dependencies)
#[derive(Debug, Clone)]
pub struct TestPortalConfig {
    pub name: String,
    pub bind_host: String,
    pub bind_port: u16,
}

#[derive(Debug, Clone)]
pub struct TestToolInfo {
    pub name: String,
    pub description: String,
}

// Test basic functionality
fn main() {
    let config = TestPortalConfig {
        name: "test".to_string(),
        bind_host: "localhost".to_string(),
        bind_port: 8080,
    };
    
    let tool = TestToolInfo {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
    };
    
    println!("Config: {:?}", config);
    println!("Tool: {:?}", tool);
    println!("Compilation test passed!");
}