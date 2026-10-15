use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("empty frame")]
    Empty,
    #[error("connection closed in the middle of a frame")]
    Truncated,
    #[error("malformed message: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Read one frame. `Ok(None)` is a clean end of stream between frames; end of stream
/// inside a frame is [`FrameError::Truncated`].
pub fn read_frame<R: Read>(reader: &mut R, max: usize) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    if len == 0 {
        return Err(FrameError::Empty);
    }
    // Grow with the data actually received rather than trusting the header up front.
    let mut body = Vec::with_capacity(len.min(8 * 1024));
    let read = reader.take(len as u64).read_to_end(&mut body)?;
    if read < len {
        return Err(FrameError::Truncated);
    }
    Ok(Some(body))
}

/// Write one frame. Fails without writing anything if `body` is empty or over `max`.
pub fn write_frame<W: Write>(writer: &mut W, body: &[u8], max: usize) -> Result<(), FrameError> {
    if body.len() > max {
        return Err(FrameError::TooLarge { len: body.len(), max });
    }
    if body.is_empty() {
        return Err(FrameError::Empty);
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge { len: body.len(), max })?;
    // One write, so concurrent writers on a datagram-ish transport never interleave a
    // header with someone else's body.
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    writer.write_all(&out)?;
    writer.flush()?;
    Ok(())
}

pub fn write_message<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
    max: usize,
) -> Result<(), FrameError> {
    write_frame(writer, &serde_json::to_vec(message)?, max)
}

/// Read and decode one message; `Ok(None)` on clean end of stream.
pub fn read_message<T: DeserializeOwned>(
    reader: &mut impl Read,
    max: usize,
) -> Result<Option<T>, FrameError> {
    match read_frame(reader, max)? {
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
        None => Ok(None),
    }
}
