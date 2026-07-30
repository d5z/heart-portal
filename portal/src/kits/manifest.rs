use serde::Deserialize;
use serde_json::Value;

/// Deserialize from a kit's manifest.json.
#[derive(Debug, Clone, Deserialize)]
pub struct KitManifest {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub platform: Option<Vec<String>>,
    pub runtime: Option<String>,
    pub command: Vec<String>,
    pub tools: Vec<KitToolDef>,
    pub permissions: Option<Vec<String>>,
    pub workspace: Option<bool>,
    /// When true, Portal pre-spawns this kit's MCP process at startup.
    pub eager: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KitToolDef {
    pub name: String,
    pub description: String,
    pub params: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_with_optional_fields() {
        let manifest: KitManifest = serde_json::from_str(
            r#"{
                "name": "hand",
                "version": "0.1.0",
                "description": "Vision tools",
                "author": "Heart",
                "platform": ["darwin", "linux"],
                "runtime": "python3",
                "command": ["python3", "-m", "hand.mcp_server"],
                "tools": [{
                    "name": "see",
                    "description": "Describe the screen",
                    "params": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                }],
                "permissions": ["screen"],
                "workspace": true
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.name, "hand");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.description.as_deref(), Some("Vision tools"));
        assert_eq!(manifest.platform.unwrap(), vec!["darwin", "linux"]);
        assert_eq!(manifest.command, vec!["python3", "-m", "hand.mcp_server"]);
        assert_eq!(manifest.tools[0].name, "see");
        assert_eq!(manifest.tools[0].params["type"], "object");
        assert_eq!(manifest.workspace, Some(true));
        assert!(manifest.eager.is_none());
    }

    #[test]
    fn parses_manifest_with_eager_true() {
        let manifest: KitManifest = serde_json::from_str(
            r#"{
                "name": "hand",
                "version": "0.1.0",
                "command": ["python3", "-m", "hand.mcp_server"],
                "tools": [{
                    "name": "see",
                    "description": "Describe the screen",
                    "params": {"type": "object"}
                }],
                "eager": true
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.name, "hand");
        assert_eq!(manifest.eager, Some(true));
    }

    #[test]
    fn parses_manifest_without_optional_fields() {
        let manifest: KitManifest = serde_json::from_str(
            r#"{
                "name": "notes",
                "version": "1.0.0",
                "command": ["node", "server.js"],
                "tools": [{
                    "name": "capture",
                    "description": "Capture a note",
                    "params": {"type": "object"}
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.name, "notes");
        assert!(manifest.description.is_none());
        assert!(manifest.platform.is_none());
        assert!(manifest.eager.is_none());
        assert_eq!(manifest.tools.len(), 1);
    }
}
