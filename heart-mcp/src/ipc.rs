//! Length-prefixed JSON over Unix domain socket (MCP supervisor clients).

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use serde::{Serialize, de::DeserializeOwned};
use anyhow::Result;

/// Wraps a UnixStream with framed message send/recv.
/// Frame format: [4-byte LE length][JSON payload]
pub struct IpcConnection {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: BufWriter<tokio::net::unix::OwnedWriteHalf>,
}

impl IpcConnection {
    /// Create from an established UnixStream.
    pub fn new(stream: UnixStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: BufWriter::new(write_half),
        }
    }

    /// Send a message (serialize → length-prefix → write).
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let json = serde_json::to_vec(msg)?;
        let len = (json.len() as u32).to_le_bytes();
        self.writer.write_all(&len).await?;
        self.writer.write_all(&json).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Receive a message (read length → read payload → deserialize).
    /// Returns None on clean EOF.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let mut len_buf = [0u8; 4];
        match self.reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len_buf) as usize;

        if len > 16 * 1024 * 1024 {
            anyhow::bail!("IPC frame too large: {} bytes", len);
        }

        let mut buf = vec![0u8; len];
        self.reader.read_exact(&mut buf).await?;
        let msg = serde_json::from_slice(&buf)?;
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_ipc::{McpRequest, McpResponse};

    #[tokio::test]
    async fn test_send_recv_roundtrip() {
        let (s1, s2) = UnixStream::pair().unwrap();
        let mut sender = IpcConnection::new(s1);
        let mut receiver = IpcConnection::new(s2);

        sender.send(&McpRequest::Ping).await.unwrap();
        let msg: Option<McpRequest> = receiver.recv().await.unwrap();
        assert!(matches!(msg, Some(McpRequest::Ping)));

        sender
            .send(&McpResponse::Pong {
                server_count: 2,
                tool_count: 7,
            })
            .await
            .unwrap();
        let msg: Option<McpResponse> = receiver.recv().await.unwrap();
        match msg {
            Some(McpResponse::Pong {
                server_count,
                tool_count,
            }) => {
                assert_eq!(server_count, 2);
                assert_eq!(tool_count, 7);
            }
            _ => panic!("Expected Pong"),
        }
    }

    #[tokio::test]
    async fn test_recv_eof() {
        let (s1, s2) = UnixStream::pair().unwrap();
        let _sender = IpcConnection::new(s1);
        let mut receiver = IpcConnection::new(s2);

        drop(_sender);

        let msg: Option<McpRequest> = receiver.recv().await.unwrap();
        assert!(msg.is_none());
    }
}
