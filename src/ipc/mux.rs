use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, oneshot};

use crate::error::{KexshError, Result};
use crate::ipc::codec::{
    read_binary_frame, write_binary_frame, write_control_frame, write_message,
};
use crate::ipc::message::{BinaryFrame, MuxRequest, MuxResponse, Request, Response};
use crate::ipc::socket_path;

/// Receive half — owned by the recv task.
pub struct LocalMuxReceiver<R> {
    reader: R,
    ctrl_tx: Arc<Mutex<Option<oneshot::Sender<MuxResponse>>>>,
}

/// Send half — owned by TuiSession main loop.
pub struct LocalMuxSender<W> {
    writer: W,
    ctrl_tx: Arc<Mutex<Option<oneshot::Sender<MuxResponse>>>>,
}

impl<R: AsyncRead + Unpin + Send> LocalMuxReceiver<R> {
    /// Read next data frame, routing Control responses internally.
    pub async fn recv_frame(&mut self) -> Result<(String, BinaryFrame)> {
        loop {
            let (tid, frame) = read_binary_frame(&mut self.reader).await?;
            if let BinaryFrame::Control(ref payload) = frame {
                if let Ok(resp) = serde_json::from_slice::<MuxResponse>(payload) {
                    let mut slot = self.ctrl_tx.lock().await;
                    if let Some(tx) = slot.take() {
                        let _ = tx.send(resp);
                    }
                }
                continue;
            }
            return Ok((tid, frame));
        }
    }
}

impl<W: AsyncWrite + Unpin + Send> LocalMuxSender<W> {
    pub async fn send_frame(&mut self, terminal_id: &str, frame: &BinaryFrame) -> Result<()> {
        write_binary_frame(&mut self.writer, terminal_id, frame).await
    }

    pub async fn send_control(&mut self, req: &MuxRequest) -> Result<MuxResponse> {
        let (tx, rx) = oneshot::channel();
        {
            let mut slot = self.ctrl_tx.lock().await;
            *slot = Some(tx);
        }
        write_control_frame(&mut self.writer, req).await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .map_err(|_| KexshError::Ipc("control response timed out".into()))?
            .map_err(|_| KexshError::Ipc("control response channel closed".into()))
    }
}

#[cfg(test)]
fn make_pair<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin + Send>(
    reader: R,
    writer: W,
) -> (LocalMuxSender<W>, LocalMuxReceiver<R>) {
    let ctrl_tx = Arc::new(Mutex::new(None));
    (
        LocalMuxSender {
            writer,
            ctrl_tx: ctrl_tx.clone(),
        },
        LocalMuxReceiver { reader, ctrl_tx },
    )
}

/// Establish a multiplex connection over a Unix socket.
pub async fn local_mux_connect(
    terminal_ids: Vec<String>,
    view_id: Option<String>,
) -> Result<(
    LocalMuxSender<tokio::net::unix::OwnedWriteHalf>,
    LocalMuxReceiver<tokio::net::unix::OwnedReadHalf>,
)> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .await
        .map_err(|e| KexshError::Ipc(format!("connect to {}: {e}", path.display())))?;

    // JSON handshake
    write_message(
        &mut stream,
        &Request::MultiplexAttach {
            terminal_ids,
            view_id,
        },
    )
    .await?;
    let resp: Response = crate::ipc::codec::read_message(&mut stream).await?;
    match resp {
        Response::MultiplexAttached { .. } => {}
        Response::Error { message } => return Err(KexshError::Ipc(message)),
        _ => return Err(KexshError::Ipc("unexpected handshake response".into())),
    }

    let (reader, writer) = stream.into_split();
    let ctrl_tx = Arc::new(Mutex::new(None));

    Ok((
        LocalMuxSender {
            writer,
            ctrl_tx: ctrl_tx.clone(),
        },
        LocalMuxReceiver { reader, ctrl_tx },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::codec::{read_binary_frame, write_binary_frame};
    use tokio::net::{UnixListener, UnixStream};

    async fn paired_streams() -> (UnixStream, UnixStream) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let client = UnixStream::connect(&sock).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn send_and_recv_data_frame() {
        let (client, server) = paired_streams().await;
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        let (mut sender, _) = make_pair(cr, cw);
        let (_, mut receiver) = make_pair(sr, sw);

        // Client sends data frame, server receives it
        sender
            .send_frame("term0001", &BinaryFrame::Data(vec![1, 2, 3]))
            .await
            .unwrap();
        let (tid, frame) = receiver.recv_frame().await.unwrap();
        assert_eq!(tid, "term0001");
        assert_eq!(frame, BinaryFrame::Data(vec![1, 2, 3]));
    }

    #[tokio::test]
    async fn send_and_recv_resize_frame() {
        let (client, server) = paired_streams().await;
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        let (mut sender, _) = make_pair(cr, cw);
        let (_, mut receiver) = make_pair(sr, sw);

        sender
            .send_frame("term0001", &BinaryFrame::Resize { cols: 80, rows: 24 })
            .await
            .unwrap();
        let (tid, frame) = receiver.recv_frame().await.unwrap();
        assert_eq!(tid, "term0001");
        assert_eq!(frame, BinaryFrame::Resize { cols: 80, rows: 24 });
    }

    #[tokio::test]
    async fn control_response_routed_to_sender() {
        let (client, server) = paired_streams().await;
        let (cr, cw) = client.into_split();
        let (mut sr, mut sw) = server.into_split();

        let ctrl_tx = Arc::new(Mutex::new(None));
        let mut sender = LocalMuxSender {
            writer: cw,
            ctrl_tx: ctrl_tx.clone(),
        };
        let mut receiver = LocalMuxReceiver {
            reader: cr,
            ctrl_tx,
        };

        // Spawn send_control — writes request, then waits for oneshot
        let ctrl_handle = tokio::spawn(async move {
            sender
                .send_control(&MuxRequest::CreateTerminal { name: None })
                .await
                .unwrap()
        });

        // Server reads the control request
        let (_tid, req_frame) = read_binary_frame(&mut sr).await.unwrap();
        assert!(matches!(req_frame, BinaryFrame::Control(_)));

        // Server sends control response followed by a data frame
        let resp = MuxResponse::TerminalCreated { id: "new1".into() };
        let payload = serde_json::to_vec(&resp).unwrap();
        write_binary_frame(&mut sw, "\0\0\0\0\0\0\0\0", &BinaryFrame::Control(payload))
            .await
            .unwrap();
        write_binary_frame(&mut sw, "term0001", &BinaryFrame::Data(vec![42]))
            .await
            .unwrap();

        // recv_frame skips the Control frame (routes to oneshot) and returns Data
        let (tid, frame) = receiver.recv_frame().await.unwrap();
        assert_eq!(tid, "term0001");
        assert_eq!(frame, BinaryFrame::Data(vec![42]));

        // send_control should have received the response via oneshot
        let resp = ctrl_handle.await.unwrap();
        assert!(matches!(resp, MuxResponse::TerminalCreated { id } if id == "new1"));
    }

    #[tokio::test]
    async fn multi_terminal_routing() {
        let (client, server) = paired_streams().await;
        // client write → server read
        let (_cr, mut cw) = client.into_split();
        let (sr, _sw) = server.into_split();
        let ctrl_tx = Arc::new(Mutex::new(None));
        let mut receiver = LocalMuxReceiver {
            reader: sr,
            ctrl_tx,
        };

        write_binary_frame(&mut cw, "term0001", &BinaryFrame::Data(b"hello".to_vec()))
            .await
            .unwrap();
        write_binary_frame(&mut cw, "term0002", &BinaryFrame::Data(b"world".to_vec()))
            .await
            .unwrap();

        let (t1, f1) = receiver.recv_frame().await.unwrap();
        let (t2, f2) = receiver.recv_frame().await.unwrap();
        assert_eq!(t1, "term0001");
        assert_eq!(f1, BinaryFrame::Data(b"hello".to_vec()));
        assert_eq!(t2, "term0002");
        assert_eq!(f2, BinaryFrame::Data(b"world".to_vec()));
    }
}
