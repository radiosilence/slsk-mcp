//! Length-prefixed framing.
//!
//! Server and peer messages carry a u32 code; peer-init and distributed
//! messages a u8. File connections carry no frames at all after the first
//! token and offset. The decoder is incremental so a connection task can feed
//! it whatever the socket produced and take out whole frames.

use bytes::{Buf, Bytes, BytesMut};

use crate::wire::{DecodeError, Writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeWidth {
    U8,
    U32,
}

impl CodeWidth {
    const fn len(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U32 => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub code: u32,
    pub body: Bytes,
}

/// Build a frame: length, code, body.
pub fn encode(width: CodeWidth, code: u32, body: &[u8]) -> Bytes {
    let mut w = Writer(BytesMut::with_capacity(4 + width.len() + body.len()));
    w.u32((width.len() + body.len()) as u32);
    match width {
        CodeWidth::U8 => w.u8(code as u8),
        CodeWidth::U32 => w.u32(code),
    };
    w.put(body);
    w.finish()
}

/// Pull one frame from the front of `buf`, if a whole one is there.
///
/// `max` bounds a single frame. It is what stops a peer announcing a
/// four-gigabyte message and the reader buffering towards it.
pub fn decode(buf: &mut BytesMut, width: CodeWidth, max: usize) -> Result<Option<Frame>, DecodeError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    if len < width.len() {
        return Err(DecodeError::Invalid("frame shorter than its code"));
    }
    if len > max {
        return Err(DecodeError::TooLarge(max));
    }
    if buf.len() < 4 + len {
        buf.reserve(4 + len - buf.len());
        return Ok(None);
    }
    buf.advance(4);
    let mut frame = buf.split_to(len).freeze();
    let code = match width {
        CodeWidth::U8 => u32::from(frame.get_u8()),
        CodeWidth::U32 => frame.get_u32_le(),
    };
    Ok(Some(Frame { code, body: frame }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_reads_reassemble() {
        let a = encode(CodeWidth::U32, 26, b"hello");
        let b = encode(CodeWidth::U32, 1, b"");
        let mut stream: Vec<u8> = [a.as_ref(), b.as_ref()].concat();
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        while !stream.is_empty() {
            let n = 3.min(stream.len());
            buf.extend_from_slice(&stream.drain(..n).collect::<Vec<_>>());
            while let Some(f) = decode(&mut buf, CodeWidth::U32, 1024).unwrap() {
                out.push(f);
            }
        }
        assert_eq!(out, [
            Frame { code: 26, body: Bytes::from_static(b"hello") },
            Frame { code: 1, body: Bytes::new() },
        ]);
    }

    #[test]
    fn an_oversized_frame_is_refused_before_buffering() {
        let mut buf = BytesMut::from(&u32::MAX.to_le_bytes()[..]);
        assert!(decode(&mut buf, CodeWidth::U32, 1 << 20).is_err());
    }
}
