use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Deserialize from a kit's manifest.json.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitManifest {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub platform: Option<Vec<String>>,
    pub runtime: Option<String>,
    #[serde(deserialize_with = "deserialize_command")]
    pub command: Vec<String>,
    pub tools: Vec<KitToolDef>,
    pub permissions: Option<Vec<String>>,
    pub workspace: Option<bool>,
    /// When true, Portal pre-spawns this kit's MCP process at startup.
    pub eager: Option<bool>,
    /// Grove setup metadata. Credentials are supplied locally, never by Grove.
    pub provision: Option<KitProvision>,
}

/// Accept the original command array and the platform-specific form
/// `{ "windows": [...], "posix": [...] }`.
fn deserialize_command<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Command {
        Legacy(Vec<String>),
        Platforms {
            windows: Option<Vec<String>>,
            posix: Option<Vec<String>>,
        },
    }

    Ok(match Command::deserialize(deserializer)? {
        Command::Legacy(values) => values,
        Command::Platforms { windows, posix } => {
            #[cfg(target_os = "windows")]
            { windows.or(posix).unwrap_or_default() }
            #[cfg(not(target_os = "windows"))]
            { posix.or(windows).unwrap_or_default() }
        }
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitProvision {
    #[serde(default, deserialize_with = "deserialize_env")]
    pub env: Vec<KitEnvVar>,
    pub auth: Option<KitAuth>,
    pub runtime: Option<KitRuntime>,
    pub install: Option<String>,
    #[serde(default)]
    pub deps: Vec<KitDependency>,
    #[serde(default)]
    pub platforms: Vec<String>,
    pub post_install: Option<String>,
    pub instructions: Option<String>,
    /// Preserve future Grove metadata when comparing/reloading manifests.
    /// Unknown fields are not echoed by the setup/status tools.
    #[serde(default, flatten)]
    pub extensions: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitRuntime {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitDependency {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub description: Option<String>,
    pub install_hint: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
}

/// Methods are alternatives (OR); requirements within a method are AND.
/// This is an additive Portal extension to Grove's existing provision metadata.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitAuth {
    #[serde(default = "auth_version")]
    pub version: u32,
    #[serde(default = "default_true")]
    pub required: bool,
    pub methods: Vec<KitAuthMethod>,
}

fn auth_version() -> u32 {
    1
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitAuthMethod {
    pub id: String,
    /// Open provider identifier. Unknown providers remain visible as unsupported.
    pub provider: String,
    pub label: Option<String>,
    /// Informational flow, e.g. api_key, basic, oauth, device_code, cli.
    pub flow: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    pub instructions: Option<String>,
    pub url: Option<String>,
    /// Unprefixed manifest tool names used for kit-managed login/status.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitEnvVar {
    #[serde(default)]
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    pub default: Option<String>,
}

// Published Grove kits use both the current list and the older name-keyed map.
// Normalize once so validation, inheritance and reload keep the same behavior.
fn deserialize_env<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<KitEnvVar>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Env {
        List(Vec<KitEnvVar>),
        Map(std::collections::BTreeMap<String, KitEnvVar>),
    }
    Ok(match Env::deserialize(deserializer)? {
        Env::List(entries) => entries,
        Env::Map(entries) => entries
            .into_iter()
            .map(|(name, mut entry)| {
                entry.name = name;
                entry
            })
            .collect(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct KitToolDef {
    pub name: String,
    pub description: String,
    pub params: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grove_legacy_env_map_preserves_requirements_and_roundtrips() {
        let provision: KitProvision = serde_json::from_value(serde_json::json!({
            "env": { "OPENAI_API_KEY": { "required": true, "description": "API key" } },
            "runtime": { "name": "bash", "version": "any" }
        }))
        .unwrap();
        assert_eq!(provision.env[0].name, "OPENAI_API_KEY");
        assert!(provision.env[0].required);
        assert_eq!(provision.env[0].description.as_deref(), Some("API key"));
        let reloaded: KitProvision =
            serde_json::from_value(serde_json::to_value(provision).unwrap()).unwrap();
        assert_eq!(reloaded.env[0].name, "OPENAI_API_KEY");
        assert!(reloaded.env[0].required);
    }

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

    #[test]
    fn selects_platform_command_variant() {
        let manifest: KitManifest = serde_json::from_value(serde_json::json!({
            "name": "platform-kit",
            "version": "1",
            "command": { "windows": ["powershell.exe"], "posix": ["sh"] },
            "tools": []
        }))
        .unwrap();
        #[cfg(target_os = "windows")]
        assert_eq!(manifest.command, ["powershell.exe"]);
        #[cfg(not(target_os = "windows"))]
        assert_eq!(manifest.command, ["sh"]);
    }
}
