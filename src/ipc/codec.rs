use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{KexshError, Result};
use crate::ipc::message::BinaryFrame;

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024; // 16 MB
const FRAME_TYPE_DATA: u8 = 0x01;
const FRAME_TYPE_RESIZE: u8 = 0x02;
const FRAME_TYPE_DETACH: u8 = 0x03;
const FRAME_TYPE_CONTROL: u8 = 0x10;

pub async fn write_message<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    msg: &T,
) -> Result<()> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_message<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(KexshError::Ipc(format!("message too large: {len} bytes")));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

pub async fn write_binary_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    terminal_id: &str,
    frame: &BinaryFrame,
) -> Result<()> {
    let type_byte = match frame {
        BinaryFrame::Data(_) => FRAME_TYPE_DATA,
        BinaryFrame::Resize { .. } => FRAME_TYPE_RESIZE,
        BinaryFrame::Detach => FRAME_TYPE_DETACH,
        BinaryFrame::Control(_) => FRAME_TYPE_CONTROL,
    };

    // Business protocol: [1B type][8B tid][payload]
    // Transport layer wraps with [4B len] prefix for TCP framing.
    // On relay to WebSocket, strip the 4B len — inner bytes are identical.
    let mut biz_header = [0u8; 9];
    biz_header[0] = type_byte;
    let tid_bytes = terminal_id.as_bytes();
    debug_assert!(
        tid_bytes.len() <= 8,
        "terminal_id exceeds 8-byte frame field: {terminal_id}"
    );
    let copy_len = tid_bytes.len().min(8);
    biz_header[1..1 + copy_len].copy_from_slice(&tid_bytes[..copy_len]);

    let resize_buf: [u8; 4];
    let payload_bytes: &[u8] = match frame {
        BinaryFrame::Data(data) => data,
        BinaryFrame::Control(payload) => payload,
        BinaryFrame::Resize { cols, rows } => {
            let [ch, cl] = cols.to_be_bytes();
            let [rh, rl] = rows.to_be_bytes();
            resize_buf = [ch, cl, rh, rl];
            &resize_buf
        }
        BinaryFrame::Detach => &[],
    };

    let total_len = (9 + payload_bytes.len()) as u32;
    stream.write_all(&total_len.to_be_bytes()).await?;
    stream.write_all(&biz_header).await?;
    stream.write_all(payload_bytes).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_binary_frame(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<(String, BinaryFrame)> {
    // Transport layer: [4B len] then [len bytes of business protocol]
    // Business protocol: [1B type][8B tid][payload]
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let total_len = u32::from_be_bytes(len_buf) as usize;

    if total_len < 9 {
        return Err(KexshError::Ipc(format!(
            "frame too short: {total_len} bytes"
        )));
    }
    if total_len > MAX_MESSAGE_SIZE {
        return Err(KexshError::Ipc(format!(
            "frame too large: {total_len} bytes"
        )));
    }

    let mut buf = vec![0u8; total_len];
    stream.read_exact(&mut buf).await?;

    let type_byte = buf[0];
    let tid = std::str::from_utf8(&buf[1..9])
        .unwrap_or("")
        .trim_end_matches('\0')
        .to_string();
    let payload = &buf[9..];

    let frame = match type_byte {
        FRAME_TYPE_DATA => BinaryFrame::Data(payload.to_vec()),
        FRAME_TYPE_RESIZE => {
            if payload.len() != 4 {
                return Err(KexshError::Ipc(format!(
                    "resize frame expects 4 bytes, got {}",
                    payload.len()
                )));
            }
            BinaryFrame::Resize {
                cols: u16::from_be_bytes([payload[0], payload[1]]),
                rows: u16::from_be_bytes([payload[2], payload[3]]),
            }
        }
        FRAME_TYPE_DETACH => BinaryFrame::Detach,
        FRAME_TYPE_CONTROL => BinaryFrame::Control(payload.to_vec()),
        _ => {
            return Err(KexshError::Ipc(format!(
                "unknown frame type: {type_byte:#x}"
            )));
        }
    };

    Ok((tid, frame))
}

pub async fn write_control_frame<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    msg: &T,
) -> Result<()> {
    let payload = serde_json::to_vec(msg)?;
    write_binary_frame(stream, "\0\0\0\0\0\0\0\0", &BinaryFrame::Control(payload)).await
}

pub async fn read_control_frame<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<T> {
    let (_, frame) = read_binary_frame(stream).await?;
    match frame {
        BinaryFrame::Control(payload) => Ok(serde_json::from_slice(&payload)?),
        other => Err(KexshError::Ipc(format!(
            "expected Control frame, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::message::{MuxRequest, MuxResponse, Request, Response};
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
    async fn roundtrip_request() {
        let (mut client, mut server) = paired_streams().await;
        let req = Request::TerminalCreate {
            name: Some("test".into()),
        };
        write_message(&mut client, &req).await.unwrap();
        let decoded: Request = read_message(&mut server).await.unwrap();
        assert!(matches!(decoded, Request::TerminalCreate { name: Some(n) } if n == "test"));
    }

    #[tokio::test]
    async fn roundtrip_response() {
        let (mut client, mut server) = paired_streams().await;
        let resp = Response::TerminalCreated {
            id: "abc123".into(),
        };
        write_message(&mut server, &resp).await.unwrap();
        let decoded: Response = read_message(&mut client).await.unwrap();
        assert!(matches!(decoded, Response::TerminalCreated { id } if id == "abc123"));
    }

    #[tokio::test]
    async fn roundtrip_view_update_layout() {
        let (mut client, mut server) = paired_streams().await;
        let req = Request::ViewUpdateLayout {
            view_id: "v1".into(),
            layout: serde_json::json!({"type": "leaf", "terminal_id": "t1"}),
            focused: "t1".into(),
        };
        write_message(&mut client, &req).await.unwrap();
        let decoded: Request = read_message(&mut server).await.unwrap();
        assert!(
            matches!(decoded, Request::ViewUpdateLayout { view_id, focused, .. } if view_id == "v1" && focused == "t1")
        );
    }

    #[tokio::test]
    async fn roundtrip_view_remove_terminal() {
        let (mut client, mut server) = paired_streams().await;
        let req = Request::ViewRemoveTerminal {
            view_id: "v1".into(),
            terminal_id: "t1".into(),
        };
        write_message(&mut client, &req).await.unwrap();
        let decoded: Request = read_message(&mut server).await.unwrap();
        assert!(
            matches!(decoded, Request::ViewRemoveTerminal { view_id, terminal_id } if view_id == "v1" && terminal_id == "t1")
        );
    }

    #[tokio::test]
    async fn roundtrip_view_attach_with_layout() {
        let (mut client, mut server) = paired_streams().await;
        let resp = Response::ViewAttach {
            terminal_ids: vec!["t1".into(), "t2".into()],
            layout: Some(serde_json::json!({"type": "split"})),
            focused: Some("t2".into()),
        };
        write_message(&mut server, &resp).await.unwrap();
        let decoded: Response = read_message(&mut client).await.unwrap();
        assert!(
            matches!(decoded, Response::ViewAttach { terminal_ids, layout: Some(_), focused: Some(f) } if terminal_ids.len() == 2 && f == "t2")
        );
    }

    #[tokio::test]
    async fn binary_frame_roundtrip_data() {
        let (mut client, mut server) = paired_streams().await;
        let frame = BinaryFrame::Data(vec![1, 2, 3, 4]);
        write_binary_frame(&mut client, "abcd1234", &frame)
            .await
            .unwrap();
        let (tid, decoded) = read_binary_frame(&mut server).await.unwrap();
        assert_eq!(tid, "abcd1234");
        assert_eq!(decoded, BinaryFrame::Data(vec![1, 2, 3, 4]));
    }

    #[tokio::test]
    async fn binary_frame_roundtrip_resize() {
        let (mut client, mut server) = paired_streams().await;
        let frame = BinaryFrame::Resize {
            cols: 120,
            rows: 40,
        };
        write_binary_frame(&mut client, "term0001", &frame)
            .await
            .unwrap();
        let (tid, decoded) = read_binary_frame(&mut server).await.unwrap();
        assert_eq!(tid, "term0001");
        assert_eq!(
            decoded,
            BinaryFrame::Resize {
                cols: 120,
                rows: 40
            }
        );
    }

    #[tokio::test]
    async fn binary_frame_roundtrip_detach() {
        let (mut client, mut server) = paired_streams().await;
        write_binary_frame(&mut client, "t1234567", &BinaryFrame::Detach)
            .await
            .unwrap();
        let (tid, decoded) = read_binary_frame(&mut server).await.unwrap();
        assert_eq!(tid, "t1234567");
        assert_eq!(decoded, BinaryFrame::Detach);
    }

    #[tokio::test]
    async fn binary_frame_empty_data() {
        let (mut client, mut server) = paired_streams().await;
        let frame = BinaryFrame::Data(vec![]);
        write_binary_frame(&mut client, "abcd1234", &frame)
            .await
            .unwrap();
        let (_, decoded) = read_binary_frame(&mut server).await.unwrap();
        assert_eq!(decoded, BinaryFrame::Data(vec![]));
    }

    #[tokio::test]
    async fn binary_frame_roundtrip_control() {
        let (mut client, mut server) = paired_streams().await;
        let payload = b"{\"test\":true}".to_vec();
        let frame = BinaryFrame::Control(payload.clone());
        write_binary_frame(&mut client, "\0\0\0\0\0\0\0\0", &frame)
            .await
            .unwrap();
        let (tid, decoded) = read_binary_frame(&mut server).await.unwrap();
        assert_eq!(tid, "");
        assert_eq!(decoded, BinaryFrame::Control(payload));
    }

    #[tokio::test]
    async fn control_frame_mux_request_roundtrip() {
        let (mut client, mut server) = paired_streams().await;
        let req = MuxRequest::CreateTerminal {
            name: Some("dev".into()),
        };
        write_control_frame(&mut client, &req).await.unwrap();
        let decoded: MuxRequest = read_control_frame(&mut server).await.unwrap();
        assert!(matches!(decoded, MuxRequest::CreateTerminal { name: Some(n) } if n == "dev"));
    }

    #[tokio::test]
    async fn control_frame_mux_response_roundtrip() {
        let (mut client, mut server) = paired_streams().await;
        let resp = MuxResponse::TerminalCreated { id: "t1".into() };
        write_control_frame(&mut client, &resp).await.unwrap();
        let decoded: MuxResponse = read_control_frame(&mut server).await.unwrap();
        assert!(matches!(decoded, MuxResponse::TerminalCreated { id } if id == "t1"));
    }

    #[tokio::test]
    async fn control_frame_empty_json() {
        let (mut client, mut server) = paired_streams().await;
        let frame = BinaryFrame::Control(b"{}".to_vec());
        write_binary_frame(&mut client, "\0\0\0\0\0\0\0\0", &frame)
            .await
            .unwrap();
        let (_, decoded) = read_binary_frame(&mut server).await.unwrap();
        assert_eq!(decoded, BinaryFrame::Control(b"{}".to_vec()));
    }

    #[tokio::test]
    async fn roundtrip_multiplex_attach() {
        let (mut client, mut server) = paired_streams().await;
        let req = Request::MultiplexAttach {
            terminal_ids: vec!["t1".into(), "t2".into()],
            view_id: Some("v1".into()),
        };
        write_message(&mut client, &req).await.unwrap();
        let decoded: Request = read_message(&mut server).await.unwrap();
        assert!(
            matches!(decoded, Request::MultiplexAttach { terminal_ids, view_id: Some(v) } if terminal_ids.len() == 2 && v == "v1")
        );
    }
}
