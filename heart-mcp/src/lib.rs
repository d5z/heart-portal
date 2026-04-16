//! Heart MCP — shared MCP protocol, connection, and client code.
//! Used by both the standalone MCP Supervisor binary and Cortex's MCP adapter.

#[cfg(unix)]
pub mod ipc;
pub mod mcp_ipc;
pub mod protocol;
pub mod connection;
pub mod client;
