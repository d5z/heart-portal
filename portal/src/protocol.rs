//! JSON-RPC 2.0 types for MCP protocol.
//! Minimal — just what Portal needs as a server.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Portal release version (from Cargo.toml at compile time).
pub const PORTAL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// Some Heart transports attach scene metadata to the request envelope.
    #[serde(default, alias = "_meta", skip_serializing_if = "serde_json::Value::is_null")]
    pub meta: serde_json::Value,
}

impl JsonRpcRequest {
    pub fn new(method: impl Into<String>, params: serde_json::Value, ids: &AtomicU64) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(ids.fetch_add(1, Ordering::SeqCst)),
            method: method.into(),
            params,
            meta: serde_json::Value::Null,
        }
    }

    pub fn notification(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: method.into(),
            params,
            meta: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
