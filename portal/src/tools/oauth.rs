//! OAuth Authorization Code + PKCE tool.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};
use url::Url;

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_SCOPE: &str = "openid profile email offline_access";
const REDIRECT_PATH: &str = "/auth/callback";
const PRIMARY_REDIRECT_PORT: u16 = 1455;
const FALLBACK_REDIRECT_PORT: u16 = 1457;
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
struct ProviderConfig {
    client_id: &'static str,
    authorize_url: &'static str,
    token_url: &'static str,
    scope: &'static str,
    redirect_path: &'static str,
    primary_redirect_port: u16,
    fallback_redirect_port: u16,
}

impl ProviderConfig {
    fn openai() -> Self {
        Self {
            client_id: OPENAI_CLIENT_ID,
            authorize_url: OPENAI_AUTHORIZE_URL,
            token_url: OPENAI_TOKEN_URL,
            scope: OPENAI_SCOPE,
            redirect_path: REDIRECT_PATH,
            primary_redirect_port: PRIMARY_REDIRECT_PORT,
            fallback_redirect_port: FALLBACK_REDIRECT_PORT,
        }
    }
}

#[derive(Debug)]
struct PkcePair {
    verifier: String,
    challenge: String,
}

#[derive(Debug)]
enum CallbackOutcome {
    Authorized { code: String },
    Continue,
    Failed(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in: Option<u64>,
    #[serde(default = "default_token_type")]
    token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

pub async fn authorize(arguments: Value) -> Result<Value> {
    let provider_name = arguments
        .get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'provider' argument"))?;

    let provider = match provider_name {
        "openai" => ProviderConfig::openai(),
        other => anyhow::bail!("Unsupported OAuth provider: {}", other),
    };

    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout_secs == 0 {
        anyhow::bail!("'timeout_secs' must be greater than 0");
    }

    let (listener, port) = bind_callback_listener(provider).await?;
    let redirect_uri = callback_redirect_uri(port, provider.redirect_path);
    let pkce = generate_pkce_pair();
    let state = random_urlsafe_string();
    let authorize_url = build_authorize_url(provider, &redirect_uri, &pkce.challenge, &state)?;

    debug!("oauth_authorize: listening on {}", redirect_uri);
    open::that(authorize_url.as_str())
        .with_context(|| "Failed to open system browser for OAuth authorization")?;

    let code = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        wait_for_callback(listener, provider.redirect_path, &state),
    )
    .await
    .map_err(|_| anyhow::anyhow!("OAuth authorization timed out after {}s", timeout_secs))??;

    let tokens = exchange_code(provider, &redirect_uri, &code, &pkce.verifier).await?;
    let text = serde_json::to_string(&tokens)?;

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    }))
}

fn generate_pkce_pair() -> PkcePair {
    let verifier = random_urlsafe_string();
    let challenge = pkce_challenge(&verifier);
    PkcePair {
        verifier,
        challenge,
    }
}

fn random_urlsafe_string() -> String {
    let bytes: [u8; 32] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn callback_redirect_uri(port: u16, path: &str) -> String {
    format!("http://localhost:{}{}", port, path)
}

fn build_authorize_url(
    provider: ProviderConfig,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> Result<String> {
    let mut url = Url::parse(provider.authorize_url)
        .with_context(|| format!("Invalid authorize URL: {}", provider.authorize_url))?;

    url.query_pairs_mut()
        .append_pair("client_id", provider.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", provider.scope)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);

    Ok(url.into())
}

async fn bind_callback_listener(provider: ProviderConfig) -> Result<(TcpListener, u16)> {
    match TcpListener::bind(("127.0.0.1", provider.primary_redirect_port)).await {
        Ok(listener) => Ok((listener, provider.primary_redirect_port)),
        Err(primary_err) => {
            warn!(
                "oauth_authorize: port {} unavailable: {}",
                provider.primary_redirect_port, primary_err
            );
            match TcpListener::bind(("127.0.0.1", provider.fallback_redirect_port)).await {
                Ok(listener) => Ok((listener, provider.fallback_redirect_port)),
                Err(fallback_err) => anyhow::bail!(
                    "Could not start OAuth callback server on localhost ports {} or {}: primary error: {}; fallback error: {}",
                    provider.primary_redirect_port,
                    provider.fallback_redirect_port,
                    primary_err,
                    fallback_err
                ),
            }
        }
    }
}

async fn wait_for_callback(
    listener: TcpListener,
    redirect_path: &str,
    expected_state: &str,
) -> Result<String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .context("Failed to accept OAuth callback request")?;

        match handle_callback_request(&mut stream, redirect_path, expected_state).await? {
            CallbackOutcome::Authorized { code } => return Ok(code),
            CallbackOutcome::Continue => continue,
            CallbackOutcome::Failed(message) => anyhow::bail!(message),
        }
    }
}

async fn handle_callback_request(
    stream: &mut TcpStream,
    redirect_path: &str,
    expected_state: &str,
) -> Result<CallbackOutcome> {
    let request = read_http_request(stream).await?;
    let Some(target) = request_target(&request) else {
        write_http_response(stream, "400 Bad Request", "Bad Request").await?;
        return Ok(CallbackOutcome::Continue);
    };

    let parsed = match Url::parse(&format!("http://localhost{}", target)) {
        Ok(url) => url,
        Err(_) => {
            write_http_response(stream, "400 Bad Request", "Bad Request").await?;
            return Ok(CallbackOutcome::Continue);
        }
    };

    if parsed.path() != redirect_path {
        write_http_response(stream, "404 Not Found", "Not Found").await?;
        return Ok(CallbackOutcome::Continue);
    }

    let mut code = None;
    let mut state = None;
    let mut oauth_error = None;
    let mut oauth_error_description = None;

    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => oauth_error = Some(value.into_owned()),
            "error_description" => oauth_error_description = Some(value.into_owned()),
            _ => {}
        }
    }

    if state.as_deref() != Some(expected_state) {
        write_http_response(
            stream,
            "400 Bad Request",
            "State mismatch. Please retry from the original authorization window.",
        )
        .await?;
        return Ok(CallbackOutcome::Continue);
    }

    if let Some(error) = oauth_error {
        let message = match oauth_error_description {
            Some(description) if !description.is_empty() => {
                format!("OAuth authorization failed: {} ({})", error, description)
            }
            _ => format!("OAuth authorization failed: {}", error),
        };
        write_http_response(stream, "400 Bad Request", "Authorization failed.").await?;
        return Ok(CallbackOutcome::Failed(message));
    }

    let Some(code) = code.filter(|value| !value.is_empty()) else {
        write_http_response(stream, "400 Bad Request", "Missing authorization code.").await?;
        return Ok(CallbackOutcome::Continue);
    };

    write_http_response(
        stream,
        "200 OK",
        "Authorization successful! You can close this window.",
    )
    .await?;
    Ok(CallbackOutcome::Authorized { code })
}

async fn read_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];

    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .context("Failed to read OAuth callback request")?;
        if n == 0 {
            break;
        }

        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > MAX_HTTP_HEADER_BYTES {
            anyhow::bail!("OAuth callback request headers exceeded {} bytes", MAX_HTTP_HEADER_BYTES);
        }
    }

    Ok(String::from_utf8_lossy(&buffer).to_string())
}

fn request_target(request: &str) -> Option<&str> {
    let request_line = request.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" {
        return None;
    }
    Some(target)
}

async fn write_http_response(stream: &mut TcpStream, status: &str, body_text: &str) -> Result<()> {
    let body = format!(
        "<!doctype html><html><body><p>{}</p></body></html>",
        html_escape(body_text)
    );
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("Failed to write OAuth callback response")?;
    stream
        .shutdown()
        .await
        .context("Failed to close OAuth callback response")?;
    Ok(())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn exchange_code(
    provider: ProviderConfig,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<TokenResponse> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to create OAuth token HTTP client")?;

    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", provider.client_id),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", code_verifier),
    ];

    let response = client
        .post(provider.token_url)
        .form(&params)
        .send()
        .await
        .context("OAuth token request failed")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read OAuth token response")?;

    if !status.is_success() {
        anyhow::bail!(
            "OAuth token exchange failed (HTTP {}): {}",
            status,
            format_token_error(&body)
        );
    }

    serde_json::from_str(&body).context("Failed to parse OAuth token response")
}

fn format_token_error(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };

    let error = value.get("error").and_then(|v| v.as_str());
    let description = value
        .get("error_description")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("message").and_then(|v| v.as_str()));

    match (error, description) {
        (Some(error), Some(description)) => format!("{}: {}", error, description),
        (Some(error), None) => error.to_string(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_pkce_pair() {
        let pkce = generate_pkce_pair();

        assert!((43..=128).contains(&pkce.verifier.len()));
        assert_eq!(pkce.challenge, pkce_challenge(&pkce.verifier));
        assert!(!pkce.verifier.contains('='));
        assert!(!pkce.challenge.contains('='));
    }

    #[test]
    fn computes_rfc7636_pkce_challenge() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn builds_openai_authorize_url() {
        let provider = ProviderConfig::openai();
        let redirect_uri = callback_redirect_uri(1455, provider.redirect_path);
        let url = build_authorize_url(provider, &redirect_uri, "challenge-value", "state-value")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(parsed.as_str().split('?').next().unwrap(), OPENAI_AUTHORIZE_URL);
        assert_eq!(query.get("client_id").unwrap(), OPENAI_CLIENT_ID);
        assert_eq!(query.get("response_type").unwrap(), "code");
        assert_eq!(query.get("redirect_uri").unwrap(), &redirect_uri);
        assert_eq!(query.get("scope").unwrap(), OPENAI_SCOPE);
        assert_eq!(query.get("code_challenge").unwrap(), "challenge-value");
        assert_eq!(query.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(query.get("state").unwrap(), "state-value");
    }
}
