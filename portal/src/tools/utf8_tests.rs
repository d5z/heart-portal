use super::*;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("portal-utf8-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Exercise the real MCP handler, including a second command on the same
/// connection after output that previously panicked inside String::truncate.
#[tokio::test]
async fn large_utf8_output_keeps_mcp_connection_alive() {
    let workspace = Workspace::new();
    let mut config = PortalConfig {
        kits_enabled: false,
        ..PortalConfig::default()
    };
    config.security.workspace_root = workspace.0.clone();
    let host = ToolHost::new(&config);
    let (client, server) = tokio::io::duplex(4096);
    let handler =
        tokio::spawn(
            async move { crate::handle_connection(server, &host, "utf8-test", None).await },
        );
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = BufReader::new(reader);
    let read_command = if cfg!(windows) {
        "type output.txt"
    } else {
        "cat output.txt"
    };
    let mut id = 0;

    for (input, expected_len, truncated) in [
        ("中".repeat(33_334), 99_999, true),
        (format!("a{}", "🙂".repeat(25_000)), 99_997, true),
        ("a".repeat(100_001), 100_000, true),
        ("🙂".repeat(25_000), 100_000, false),
        ("中文🙂".into(), 10, false),
    ] {
        std::fs::write(workspace.0.join("output.txt"), &input).unwrap();
        for (tool, args, is_error) in [
            (
                "portal_exec",
                serde_json::json!({"command": read_command}),
                false,
            ),
            (
                "portal_exec",
                serde_json::json!({"command": format!("{read_command} >&2; exit 7")}),
                true,
            ),
            (
                "portal_file_read",
                serde_json::json!({"path": "output.txt"}),
                false,
            ),
        ] {
            // cmd.exe uses '&', whereas POSIX shells use ';'.
            let args = if cfg!(windows) && is_error {
                serde_json::json!({"command": format!("{read_command} >&2 & exit /b 7")})
            } else {
                args
            };
            id += 1;
            let request = serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": tool, "arguments": args}
            });
            writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            let size = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                .await
                .expect("MCP response timed out")
                .unwrap();
            assert!(size > 0, "MCP connection closed after {tool}");
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["id"], id);
            assert!(reply.get("error").is_none(), "{reply}");
            let result = &reply["result"];
            let text = result["content"][0]["text"].as_str().unwrap();
            if is_error {
                assert_eq!(result["isError"], true);
                assert!(text.starts_with("\n--- stderr ---\n"));
            } else {
                assert_eq!(result["truncated"], truncated);
                assert!(text.starts_with(&input[..expected_len]));
                if truncated {
                    assert!(text[expected_len..].contains("truncated"));
                } else {
                    assert_eq!(text, input);
                }
            }

            id += 1;
            let next = serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": "portal_exec", "arguments": {"command": "echo still-alive"}}
            });
            writer
                .write_all(format!("{next}\n").as_bytes())
                .await
                .unwrap();
            line.clear();
            tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                .await
                .expect("second command timed out")
                .unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["id"], id);
            assert_eq!(reply["result"]["isError"], false);
            assert_eq!(
                reply["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .trim(),
                "still-alive"
            );
        }
    }
    handler.abort();
    let _ = handler.await;
}

/// stderr is diagnostic output, not a failure flag. Exercise the tool boundary
/// so successful diagnostics and silent nonzero exits cannot be misclassified.
#[tokio::test]
async fn exec_status_uses_exit_code_and_retains_stderr() {
    let workspace = Workspace::new();
    let mut config = PortalConfig {
        kits_enabled: false,
        ..PortalConfig::default()
    };
    config.security.workspace_root = workspace.0.clone();
    let host = ToolHost::new(&config);
    for (code, diagnostics) in [(0, true), (7, true), (7, false)] {
        let command = if cfg!(windows) {
            format!(
                "{}exit /b {code}",
                if diagnostics {
                    "echo diagnostic 1>&2 & "
                } else {
                    ""
                }
            )
        } else {
            format!(
                "{}exit {code}",
                if diagnostics {
                    "echo diagnostic >&2; "
                } else {
                    ""
                }
            )
        };
        let result = host
            .call("portal_exec", serde_json::json!({"command": command}))
            .await
            .unwrap();
        assert_eq!(result["isError"], code != 0, "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(text.contains("--- stderr ---"), diagnostics, "{text}");
        assert_eq!(text.contains("diagnostic"), diagnostics, "{text}");
        assert_eq!(text.contains("exit code: 7"), code == 7, "{text}");
    }
}
