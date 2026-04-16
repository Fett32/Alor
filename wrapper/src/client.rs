use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::Envelope;

pub struct DaemonClient {
    writer: tokio::net::unix::OwnedWriteHalf,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
}

impl DaemonClient {
    /// Connect to the daemon socket at `path`.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self> {
        let stream = UnixStream::connect(path.as_ref())
            .await
            .with_context(|| format!("cannot connect to daemon socket {:?}", path.as_ref()))?;
        let (read_half, write_half) = stream.into_split();
        Ok(DaemonClient {
            writer: write_half,
            reader: BufReader::new(read_half),
        })
    }

    /// Send an envelope to the daemon (newline-delimited JSON).
    pub async fn send(&mut self, envelope: &Envelope) -> Result<()> {
        let mut line = serde_json::to_string(envelope).context("serialize Envelope")?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .context("write to daemon socket")?;
        self.writer
            .flush()
            .await
            .context("flush daemon socket")?;
        Ok(())
    }

    /// Read the next newline-delimited JSON envelope from the daemon.
    /// Returns `None` on EOF (daemon closed the connection).
    pub async fn recv(&mut self) -> Result<Option<Envelope>> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .context("read from daemon socket")?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let env: Envelope =
            serde_json::from_str(line.trim()).context("deserialize Envelope")?;
        Ok(Some(env))
    }
}
