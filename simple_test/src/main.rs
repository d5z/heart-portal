use serde_json::{json, Value, to_value};
use anyhow::Result;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    let tool = ToolInfo {
        name: "test".to_string(),
        description: "test tool".to_string(),
        input_schema: json!({"type": "object"}),
    };
    println!("Tool: {:?}", tool);
    
    // Test reqwest
    let client = reqwest::Client::new();
    println!("Reqwest client created");
    
    // Test toml
    let config: toml::Value = toml::from_str("key = 'value'")?;
    println!("TOML parsed: {:?}", config);
    
    Ok(())
}