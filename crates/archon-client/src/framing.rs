//! NDJSON framing shared by the Archon client and the daemon's server:
//! one JSON value per `\n`-terminated line, capped at `MAX_FRAME_BYTES`.

use archon_core::error::ArchonError;
use archon_core::protocol::MAX_FRAME_BYTES;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// One frame as the bytes that go on the wire, newline included.
pub fn encode_frame<T: Serialize>(frame: &T) -> Result<Vec<u8>, ArchonError> {
    let mut line = serde_json::to_vec(frame)?;
    assert!(
        !line.contains(&b'\n'),
        "serde_json never emits a raw newline inside a value"
    );
    if line.len() > MAX_FRAME_BYTES {
        return Err(ArchonError::Protocol(format!(
            "frame of {} bytes exceeds the {MAX_FRAME_BYTES}-byte cap",
            line.len()
        )));
    }
    line.push(b'\n');
    Ok(line)
}

pub async fn write_frame<W, T>(writer: &mut W, frame: &T) -> Result<(), ArchonError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let line = encode_frame(frame)?;
    writer
        .write_all(&line)
        .await
        .map_err(|e| ArchonError::io("writing a frame", e))?;
    writer
        .flush()
        .await
        .map_err(|e| ArchonError::io("flushing a frame", e))
}

/// `Ok(None)` on a clean end of stream between frames.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, ArchonError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let mut line = Vec::new();
    let limit = (MAX_FRAME_BYTES + 1) as u64;
    let read = tokio::io::AsyncReadExt::take(&mut *reader, limit)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|e| ArchonError::io("reading a frame", e))?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        let why = match line.len() > MAX_FRAME_BYTES {
            true => format!("frame exceeds the {MAX_FRAME_BYTES}-byte cap"),
            false => "connection closed mid-frame".to_owned(),
        };
        return Err(ArchonError::Protocol(why));
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|e| ArchonError::Protocol(format!("malformed frame: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use archon_core::protocol::{ClientFrame, Request};

    #[tokio::test]
    async fn frames_round_trip_and_eof_is_clean() {
        let frame = ClientFrame {
            id: 1,
            request: Request::Status(Default::default()),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).await.unwrap();
        assert_eq!(buf.last(), Some(&b'\n'));
        let mut reader = tokio::io::BufReader::new(buf.as_slice());
        let back: Option<ClientFrame> = read_frame(&mut reader).await.unwrap();
        assert_eq!(back, Some(frame));
        let end: Option<ClientFrame> = read_frame(&mut reader).await.unwrap();
        assert_eq!(end, None);
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error() {
        let mut reader = tokio::io::BufReader::new(&b"{\"req\":1"[..]);
        let result: Result<Option<ClientFrame>, _> = read_frame(&mut reader).await;
        assert!(matches!(result, Err(ArchonError::Protocol(_))));
    }
}
