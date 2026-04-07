// Minimal test to check basic compilation
use serde_json::{json, Value};
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

fn main() -> Result<()> {
    let tool = ToolInfo {
        name: "test".to_string(),
        description: "test tool".to_string(),
        input_schema: json!({"type": "object"}),
    };
    println!("Tool: {:?}", tool);
    Ok(())
}