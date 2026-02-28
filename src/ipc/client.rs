use tokio::net::UnixStream;

use crate::error::Result;
use crate::ipc::codec::{read_message, write_message};
use crate::ipc::message::{Request, Response};
use crate::ipc::socket_path;

pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    pub async fn connect() -> Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).await.map_err(|e| {
            crate::error::KexshError::Ipc(format!("failed to connect to {}: {e}", path.display()))
        })?;
        Ok(Self { stream })
    }

    pub async fn send(&mut self, req: Request) -> Result<Response> {
        write_message(&mut self.stream, &req).await?;
        read_message(&mut self.stream).await
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}
