//! Authenticated, bounded native calls to the Android app. No shell fallback.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub fn configured() -> bool {
    std::env::var_os("HEART_ANDROID_PORT").is_some()
        && std::env::var_os("HEART_ANDROID_TOKEN").is_some()
}

async fn request(mut body: Value) -> Result<Value> {
    let port: u16 = std::env::var("HEART_ANDROID_PORT")?.parse()?;
    body["token"] = Value::String(std::env::var("HEART_ANDROID_TOKEN")?);
    tokio::time::timeout(std::time::Duration::from_secs(25), async {
        let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let mut bytes = serde_json::to_vec(&body)?;
        anyhow::ensure!(bytes.len() <= 262144, "Android request too large");
        bytes.push(b'\n'); stream.write_all(&bytes).await?;
        let mut response = Vec::new();
        BufReader::new(stream.take(1048577)).read_until(b'\n', &mut response).await?;
        anyhow::ensure!(response.len() <= 1048576 && response.last() == Some(&b'\n'), "Invalid Android bridge response");
        let value: Value = serde_json::from_slice(&response)?;
        if let Some(error) = value.get("error") { anyhow::bail!("Android tool: {}", error); }
        value.get("result").cloned().context("Missing Android result")
    }).await.context("Android operation timed out; check activity before retrying a write with the same request_id")?
}

pub async fn list() -> Vec<crate::tools::ToolInfo> {
    if !configured() {
        return vec![];
    }
    let Ok(value) = request(json!({"method":"list"})).await else {
        return vec![];
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(crate::tools::ToolInfo {
                name: item.get("name")?.as_str()?.to_owned(),
                description: item.get("description")?.as_str()?.to_owned(),
                input_schema: item.get("inputSchema")?.clone(),
            })
        })
        .collect()
}

pub async fn call(name: &str, arguments: Value) -> Result<Value> {
    let result = request(json!({"method":"call", "name":name, "arguments":arguments})).await?;
    Ok(
        json!({"content":[{"type":"text", "text":serde_json::to_string(&result)?}], "structuredContent": result}),
    )
}
