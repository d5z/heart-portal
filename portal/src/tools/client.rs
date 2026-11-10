//! Client commands are data requests, never shell commands.
use anyhow::{Context, Result};
use std::{future::Future, path::PathBuf, pin::Pin};

// Reserve only the desktop protocol verbs. Other @ prefixes are shell syntax
// (cmd's @echo, PowerShell arrays/hashtables), including in client-managed runs.
pub fn parse_command(command: &str) -> Option<(&str, &str)> {
    let command = command.trim_start().strip_prefix('@')?;
    let (verb, args) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    matches!(verb, "context" | "scenes" | "plugins" | "security-approve").then_some((verb, args.trim()))
}

pub trait ClientHandler: Send + Sync {
    fn handle_client_command<'a>(
        &'a self,
        verb: &'a str,
        args: &'a str,
        scene_id: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

pub struct NoClientHandler;
impl ClientHandler for NoClientHandler {
    fn handle_client_command<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async { anyhow::bail!("no client handler registered") })
    }
}

/// Desktop publishes a rotating local capability. Re-read it on every call so
/// a supervised Portal can survive Desktop restarts without retaining a token.
pub struct DesktopClientHandler {
    pub file: PathBuf,
    pub endpoint: String,
}
impl ClientHandler for DesktopClientHandler {
    fn handle_client_command<'a>(
        &'a self,
        verb: &'a str,
        args: &'a str,
        scene_id: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let bytes = tokio::fs::read(&self.file)
                .await
                .context("Desktop client unavailable")?;
            anyhow::ensure!(bytes.len() < 4096, "Invalid Desktop client registration");
            let registration: serde_json::Value = serde_json::from_slice(&bytes)?;
            let port = registration["port"]
                .as_u64()
                .filter(|p| *p > 0 && *p <= 65535)
                .context("Invalid client port")?;
            let token = registration["token"]
                .as_str()
                .context("Missing client token")?;
            let response = reqwest::Client::builder()
                .no_proxy().redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(if verb == "security-approve" { 125 } else { 10 })).build()?
                .post(format!("http://127.0.0.1:{port}/command"))
                .bearer_auth(token)
                .json(&serde_json::json!({"verb": verb, "args": args, "sceneId": scene_id, "endpoint": self.endpoint}))
                .send().await.context("Desktop client unavailable")?;
            anyhow::ensure!(
                response.status().is_success(),
                "Desktop client rejected request ({})",
                response.status()
            );
            let value: serde_json::Value = response.json().await?;
            if let Some(error) = value["error"].as_str() {
                anyhow::bail!("{error}");
            }
            Ok(value["text"]
                .as_str()
                .context("Invalid client response")?
                .to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PortalConfig, tools::ToolHost};
    use serde_json::json;
    use std::sync::Arc;

    struct Echo;
    impl ClientHandler for Echo {
        fn handle_client_command<'a>(
            &'a self,
            verb: &'a str,
            args: &'a str,
            scene: Option<&'a str>,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move { Ok(format!("{verb}|{args}|{}", scene.unwrap_or("none"))) })
        }
    }

    #[tokio::test]
    async fn shell_at_prefixes_work_with_and_without_a_desktop_handler() {
        for command in ["@echo marker", "@(1,2) | Write-Output", "@{key=1}", "@unknown"] {
            assert!(parse_command(command).is_none());
        }
        let mut config = PortalConfig::default();
        config.security.workspace_root = std::env::temp_dir();
        for host in [ToolHost::new(&config), ToolHost::new(&config).with_client_handler(Arc::new(Echo))] {
            let command = if cfg!(windows) { "@echo shell-marker" } else { "@not-a-command 2>/dev/null || printf shell-marker" };
            let result = host.call("portal_exec", json!({"command":command})).await.unwrap();
            assert_eq!(result["isError"], false);
            assert!(result["content"][0]["text"].as_str().unwrap().contains("shell-marker"));
            #[cfg(windows)] {
                let result = host.call("portal_exec", json!({"command":"@(1,2) | Write-Output", "shell":"powershell"})).await.unwrap();
                assert_eq!(result["isError"], false);
                assert!(result["content"][0]["text"].as_str().unwrap().contains("1"));
            }
        }
    }

    #[tokio::test]
    async fn dispatches_client_commands_and_preserves_metadata() {
        let config = PortalConfig::default();
        let host = ToolHost::new(&config).with_client_handler(Arc::new(Echo));
        for (command, expected) in [
            ("@context scene-b", "context|scene-b|scene-a"),
            ("  @context\t scene-b  ", "context|scene-b|scene-a"),
            ("@scenes", "scenes||scene-a"),
            ("@context", "context||scene-a"),
            (
                "@plugins ; echo forbidden",
                "plugins|; echo forbidden|scene-a",
            ),
        ] {
            let request = serde_json::from_value(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"portal_exec", "arguments":{"command":command,"background":true}, "_meta":{"scene_id":"scene-a"}}})).unwrap();
            let response = crate::handle_request(&request, &host, "test").await;
            assert_eq!(response.result.unwrap()["content"][0]["text"], expected);
        }
    }

    #[tokio::test]
    async fn unregistered_and_disabled_commands_never_fall_through_to_shell() {
        let mut config = PortalConfig::default();
        let host = ToolHost::new(&config);
        assert!(host
            .call("portal_exec", json!({"command":"@context"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("no client handler"));
        config.tools.exec = false;
        let host = ToolHost::new(&config).with_client_handler(Arc::new(Echo));
        assert!(host
            .call("portal_exec", json!({"command":"@scenes"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled"));
    }
    #[tokio::test]
    async fn desktop_transport_authenticates_and_reloads_registration() {
        use axum::{http::HeaderMap, routing::post, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/command", post(|headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-capability");
                assert_eq!(body, json!({"verb":"context", "args":"scene-b", "sceneId":"scene-a", "endpoint":"https://fixture.test/a"}));
                Json(json!({"text":"跨场景历史"}))
            }))).await.unwrap();
        });
        let file =
            std::env::temp_dir().join(format!("portal-client-{}.json", uuid::Uuid::new_v4()));
        tokio::fs::write(
            &file,
            json!({"port":port,"token":"fixture-capability"}).to_string(),
        )
        .await
        .unwrap();
        let handler = DesktopClientHandler {
            file: file.clone(),
            endpoint: "https://fixture.test/a".into(),
        };
        assert_eq!(
            handler
                .handle_client_command("context", "scene-b", Some("scene-a"))
                .await
                .unwrap(),
            "跨场景历史"
        );
        tokio::fs::remove_file(file).await.unwrap();
        assert!(handler
            .handle_client_command("context", "scene-b", None)
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable"));
        server.abort();
    }
}
