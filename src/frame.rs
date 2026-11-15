use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const OPEN: u8 = 0;
pub const STDIN: u8 = 1;
pub const STDIN_EOF: u8 = 2;
pub const STDOUT: u8 = 3;
pub const STDERR: u8 = 4;
pub const EXIT: u8 = 5;
pub const ERROR: u8 = 6;
pub const CONTROL: u8 = 7;
pub const MAX_PAYLOAD: usize = 65_536;
pub const MAX_OPEN: usize = 8_192;
/// The agent, not this layer, decides which CONTROL requests exist, so a request it does not
/// know is read in full and answered with an ERROR instead of a dropped connection.
const MAX_CONTROL: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub tag: u8,
    pub payload: Vec<u8>,
}

pub fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn validate_header(tag: u8, len: usize) -> io::Result<()> {
    if tag > CONTROL {
        return Err(invalid("unknown frame tag"));
    }
    if len > MAX_PAYLOAD || (tag == OPEN && len > MAX_OPEN) {
        return Err(invalid("frame payload too long"));
    }
    if (tag == STDIN_EOF && len != 0)
        || (tag == EXIT && len != 4)
        || (tag == CONTROL && !(1..=MAX_CONTROL).contains(&len))
    {
        return Err(invalid("invalid frame payload length"));
    }
    Ok(())
}

fn validate_text(tag: u8, payload: &[u8]) -> io::Result<()> {
    if tag == OPEN || tag == ERROR || tag == CONTROL {
        std::str::from_utf8(payload).map_err(|_| invalid("frame payload is not UTF-8"))?;
    }
    Ok(())
}

pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut tag = [0];
    if reader.read(&mut tag).await? == 0 {
        return Ok(None);
    }
    let len = reader.read_u32().await? as usize;
    validate_header(tag[0], len)?;
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload).await?;
    validate_text(tag[0], &payload)?;
    Ok(Some(Frame {
        tag: tag[0],
        payload,
    }))
}

pub async fn write<W: AsyncWrite + Unpin>(
    writer: &mut W,
    tag: u8,
    payload: &[u8],
) -> io::Result<()> {
    validate_header(tag, payload.len())?;
    validate_text(tag, payload)?;
    let mut header = [tag, 0, 0, 0, 0];
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn encodes_big_endian_header_and_binary_payload() {
        let mut bytes = Vec::new();
        write(&mut bytes, STDOUT, &[0, 255, 10]).await.unwrap();
        assert_eq!(bytes, [3, 0, 0, 0, 3, 0, 255, 10]);
        let frame = read(&mut bytes.as_slice()).await.unwrap().unwrap();
        assert_eq!(frame.tag, STDOUT);
        assert_eq!(frame.payload, [0, 255, 10]);
    }

    #[tokio::test]
    async fn accepts_maximum_payload_and_open_lengths() {
        for (tag, size) in [(STDIN, MAX_PAYLOAD), (OPEN, MAX_OPEN)] {
            let mut bytes = Vec::new();
            write(&mut bytes, tag, &vec![b'a'; size]).await.unwrap();
            assert_eq!(
                read(&mut bytes.as_slice())
                    .await
                    .unwrap()
                    .unwrap()
                    .payload
                    .len(),
                size
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_headers_before_reading_payload() {
        for header in [
            [8, 0, 0, 0, 0],
            [STDIN, 0, 1, 0, 1],
            [OPEN, 0, 0, 32, 1],
            [STDIN_EOF, 0, 0, 0, 1],
            [EXIT, 0, 0, 0, 3],
        ] {
            let err = read(&mut header.as_slice()).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{header:?}");
        }
    }

    #[tokio::test]
    async fn distinguishes_clean_eof_from_truncated_frame() {
        assert_eq!(read(&mut &b""[..]).await.unwrap(), None);
        for bytes in [&b"\x01"[..], &b"\x01\0\0\0\x02x"[..]] {
            assert_eq!(
                read(&mut &bytes[..]).await.unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    #[tokio::test]
    async fn rejects_non_utf8_open_and_error() {
        for tag in [OPEN, ERROR] {
            let bytes = [tag, 0, 0, 0, 1, 255];
            assert_eq!(
                read(&mut bytes.as_slice()).await.unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[tokio::test]
    async fn control_requests_are_short_utf8_text() {
        for payload in [
            b"reload".as_slice(),
            b"status-detail".as_slice(),
            b"x".as_slice(),
            b"status-summary".as_slice(),
            &[b'a'; MAX_CONTROL],
        ] {
            let mut bytes = Vec::new();
            write(&mut bytes, CONTROL, payload).await.unwrap();
            assert_eq!(
                read(&mut bytes.as_slice()).await.unwrap().unwrap().payload,
                payload
            );
        }
        for payload in [
            b"".as_slice(),
            &[b'a'; MAX_CONTROL + 1],
            b"status\xff".as_slice(),
        ] {
            assert!(write(&mut Vec::new(), CONTROL, payload).await.is_err());
        }
    }
}
