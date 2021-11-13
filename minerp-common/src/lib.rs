use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub type AuthKey = [u8; 32];

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Message {
    SwitchToNetty,
    Authenticate(AuthKey),
    IncomingConnection(AuthKey),
    AssignedDomain(String),
    RequestDomain(Option<String>),
}

impl Message {
    pub async fn read_from(
        stream: &mut (impl tokio::io::AsyncRead + std::marker::Unpin),
    ) -> anyhow::Result<Self> {
        let length = stream.read_u32().await?;
        let mut buf = vec![0u8; length as usize];
        stream.read_exact(&mut buf).await?;
        Ok(bincode::deserialize(&buf)?)
    }
    pub async fn write_to(
        &self,
        stream: &mut (impl tokio::io::AsyncWrite + std::marker::Unpin),
    ) -> anyhow::Result<()> {
        let buf = bincode::serialize(&self)?;
        stream.write_u32(buf.len() as u32).await?;
        stream.write_all(&buf).await?;
        stream.flush().await?;
        Ok(())
    }
}
