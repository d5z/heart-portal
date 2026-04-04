use async_trait::async_trait;
use anyhow::Result;
use crate::{ToolResult, ToolSpec};
use serde_json::Value;
use std::collections::HashMap;

/// Tool trait for implementing executable tools
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the name of this tool
    fn name(&self) -> &str;
    
    /// Get the description of this tool
    fn description(&self) -> &str;
    
    /// Get the JSON schema for this tool's parameters
    fn parameters_schema(&self) -> Value;
    
    /// Execute the tool with the given arguments
    async fn execute(&self, arguments: Value) -> Result<ToolResult>;
}

/// Registry for managing available tools
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new empty tool registry
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a new tool
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Get a tool by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// List all tool specifications for LLM consumption
    pub fn list_specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
            })
            .collect()
    }

    /// OpenAI-compatible tool specs (type: "function" wrapper)
    pub fn specs(&self) -> Vec<serde_json::Value> {
        self.list_specs()
            .into_iter()
            .map(|spec| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.parameters,
                    }
                })
            })
            .collect()
    }

    /// Execute a tool by name with arguments
    pub async fn execute(&self, name: &str, arguments: Value) -> Result<ToolResult> {
        match self.get(name) {
            Some(tool) => tool.execute(arguments).await,
            None => Ok(ToolResult::error(format!("Tool '{}' not found", name))),
        }
    }

    /// Get list of all tool names
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Get the number of registered tools
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tool_count", &self.tools.len())
            .field("tool_names", &self.tool_names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolResult;
    use serde_json::json;

    struct MockTool {
        name: String,
        description: String,
        should_fail: bool,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str { &self.name }
        fn description(&self) -> &str { &self.description }
        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "input": { "type": "string", "description": "Test input" }
                },
                "required": ["input"]
            })
        }
        async fn execute(&self, arguments: Value) -> Result<ToolResult> {
            if self.should_fail {
                return Ok(ToolResult::error("Mock tool failed"));
            }
            let input = arguments.get("input").and_then(|v| v.as_str()).unwrap_or("no input");
            Ok(ToolResult::success(format!("Mock tool executed with: {}", input)))
        }
    }

    #[tokio::test]
    async fn test_tool_registry_basic() {
        let mut registry = ToolRegistry::new();
        assert!(registry.is_empty());
        registry.register(Box::new(MockTool {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            should_fail: false,
        }));
        assert_eq!(registry.len(), 1);
        assert!(registry.get("test_tool").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_tool_registry_execute() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(MockTool {
            name: "ok".to_string(), description: "ok".to_string(), should_fail: false,
        }));
        let r = registry.execute("ok", json!({"input": "hi"})).await.unwrap();
        assert!(r.success);
        let r = registry.execute("nope", json!({})).await.unwrap();
        assert!(!r.success);
    }
}
