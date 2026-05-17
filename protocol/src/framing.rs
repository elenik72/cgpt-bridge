//! Chrome Native Messaging framing, reused for our local UDS channel.
//!
//! Wire format per frame:
//!
//! ```text
//! [ 4 bytes little-endian u32 length ][ N bytes UTF-8 JSON ]
//! ```
//!
//! Chrome itself caps host→extension frames at 1 MiB. We mirror that for our
//! own UDS channel so a malicious or buggy peer cannot make us allocate
//! gigabytes for a single message.

use std::io::{self, Read, Write};

/// Maximum bytes we will allocate for a single inbound frame.
pub const MAX_FRAME_BYTES: u32 = 1_024 * 1_024;

#[derive(Debug)]
pub enum ReadFrameError {
    /// Clean EOF on the length prefix. Callers should treat this as a normal
    /// end-of-stream, not an error to report.
    Eof,
    Io(io::Error),
    OversizeFrame {
        len: u32,
        cap: u32,
    },
}

impl From<io::Error> for ReadFrameError {
    fn from(e: io::Error) -> Self {
        ReadFrameError::Io(e)
    }
}

impl std::fmt::Display for ReadFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadFrameError::Eof => write!(f, "eof"),
            ReadFrameError::Io(e) => write!(f, "io: {}", e),
            ReadFrameError::OversizeFrame { len, cap } => {
                write!(f, "oversize frame: {} bytes (cap {})", len, cap)
            }
        }
    }
}

impl std::error::Error for ReadFrameError {}

/// Reads exactly one frame. Returns the raw JSON bytes without the length
/// prefix. Clean EOF on the prefix becomes `Err(ReadFrameError::Eof)` so the
/// caller can exit normally.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, ReadFrameError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(ReadFrameError::Eof);
        }
        Err(e) => return Err(ReadFrameError::Io(e)),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ReadFrameError::OversizeFrame {
            len,
            cap: MAX_FRAME_BYTES,
        });
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    Ok(body)
}

/// Writes one frame. `body` must already be valid UTF-8 JSON; this function
/// does not validate that. We do enforce the size cap.
pub fn write_frame<W: Write>(writer: &mut W, body: &[u8]) -> io::Result<()> {
    if body.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame body {} bytes exceeds cap {}",
                body.len(),
                MAX_FRAME_BYTES
            ),
        ));
    }
    let len = (body.len() as u32).to_le_bytes();
    writer.write_all(&len)?;
    writer.write_all(body)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, br#"{"hello":1}"#).unwrap();
        let mut cur = Cursor::new(buf);
        let body = read_frame(&mut cur).unwrap();
        assert_eq!(body, br#"{"hello":1}"#);
    }

    #[test]
    fn eof_on_empty_input() {
        let mut cur = Cursor::new(Vec::new());
        match read_frame(&mut cur) {
            Err(ReadFrameError::Eof) => {}
            other => panic!("expected Eof, got {:?}", other),
        }
    }

    #[test]
    fn rejects_oversize_read() {
        let oversize: u32 = MAX_FRAME_BYTES + 1;
        let mut bytes = oversize.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 8]);
        let mut cur = Cursor::new(bytes);
        match read_frame(&mut cur) {
            Err(ReadFrameError::OversizeFrame { len, cap }) => {
                assert_eq!(len, oversize);
                assert_eq!(cap, MAX_FRAME_BYTES);
            }
            other => panic!("expected OversizeFrame, got {:?}", other),
        }
    }

    #[test]
    fn rejects_oversize_write() {
        let mut sink: Vec<u8> = Vec::new();
        // Construct a body just over the cap. We don't actually allocate
        // MAX_FRAME_BYTES + 1 bytes here — we just check the length-check
        // path with a fake slice via std::iter::repeat.
        let big: Vec<u8> = vec![b'x'; (MAX_FRAME_BYTES as usize) + 1];
        let err = write_frame(&mut sink, &big).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
