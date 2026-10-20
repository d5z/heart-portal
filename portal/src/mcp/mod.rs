pub use client::McpClient;
pub use connection::{McpConnection, McpServerConfig};
pub(crate) use connection::{RemoteError, RequestRejected, RequestTimeout};
pub use protocol::McpToolInfo;

mod cancellation;
mod client;
mod connection;
pub(crate) mod limits;
mod ownership;
pub(crate) use ownership::ProcessOwner;
mod protocol;
#[cfg(windows)]
mod windows_job;
