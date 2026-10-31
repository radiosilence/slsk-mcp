//! A TCP stream with a read buffer, so whatever arrives after a frame — a
//! pipelined message, or the raw token at the start of a file connection — is
//! kept for the next reader rather than lost.

use std::io;
use std::time::Duration;

use bytes::{Buf, BytesMut};
use slsk_proto::{CodeWidth, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

pub const READ_CHUNK: usize = 256 * 1024;

pub struct Conn {
    pub stream: TcpStream,
    pub buf: BytesMut,
}

impl Conn {
    pub fn new(stream: TcpStream) -> Self {
        let _ = stream.set_nodelay(true);
        Self {
            stream,
            buf: BytesMut::with_capacity(8 * 1024),
        }
    }

    pub async fn frame(&mut self, width: CodeWidth, max: usize) -> io::Result<Option<Frame>> {
        read_frame(&mut self.stream, &mut self.buf, width, |_| max).await
    }

    pub async fn read_exact_buffered(&mut self, n: usize) -> io::Result<BytesMut> {
        while self.buf.len() < n {
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(self.buf.split_to(n))
    }

    pub async fn u32(&mut self) -> io::Result<u32> {
        let b = self.read_exact_buffered(4).await?;
        Ok(u32::from_le_bytes(b[..].try_into().unwrap()))
    }

    pub async fn u64(&mut self) -> io::Result<u64> {
        let b = self.read_exact_buffered(8).await?;
        Ok(u64::from_le_bytes(b[..].try_into().unwrap()))
    }

    pub fn split(self) -> (Reader, OwnedWriteHalf) {
        let (r, w) = self.stream.into_split();
        (
            Reader {
                half: r,
                buf: self.buf,
            },
            w,
        )
    }
}

pub struct Reader {
    pub half: OwnedReadHalf,
    pub buf: BytesMut,
}

impl Reader {
    pub async fn frame(&mut self, width: CodeWidth, max: usize) -> io::Result<Option<Frame>> {
        read_frame(&mut self.half, &mut self.buf, width, |_| max).await
    }

    /// As [`Reader::frame`], with the bound chosen by the frame's code, which
    /// is known from its header before any of the body is buffered.
    pub async fn frame_by(
        &mut self,
        width: CodeWidth,
        max: impl Fn(u32) -> usize,
    ) -> io::Result<Option<Frame>> {
        read_frame(&mut self.half, &mut self.buf, width, max).await
    }

    /// Wait until some of the next frame has arrived. False at a clean end of
    /// stream.
    pub async fn ready(&mut self) -> io::Result<bool> {
        if !self.buf.is_empty() {
            return Ok(true);
        }
        self.buf.reserve(8 * 1024);
        Ok(self.half.read_buf(&mut self.buf).await? > 0)
    }
}

async fn read_frame<R: AsyncReadExt + Unpin>(
    r: &mut R,
    buf: &mut BytesMut,
    width: CodeWidth,
    max: impl Fn(u32) -> usize,
) -> io::Result<Option<Frame>> {
    let header = 4 + match width {
        CodeWidth::U8 => 1,
        CodeWidth::U32 => 4,
    };
    loop {
        let limit = match buf.get(4..header) {
            Some([c]) => max(u32::from(*c)),
            Some(c) => max(u32::from_le_bytes(c.try_into().unwrap())),
            None => usize::MAX,
        };
        match slsk_proto::frame::decode(buf, width, limit) {
            Ok(Some(f)) => return Ok(Some(f)),
            Ok(None) => {}
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        }
        // A length is only a claim: grow as bytes arrive, not to it at once.
        if buf.capacity() - buf.len() < 4096 {
            buf.reserve(64 * 1024);
        }
        if r.read_buf(buf).await? == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            buf.advance(buf.len());
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
}

/// Drain `rx` onto the socket. The channel is bounded by the caller, so a
/// peer that stops reading fills it and further sends fail fast instead of
/// queueing in memory.
pub fn spawn_writer(mut half: OwnedWriteHalf, mut rx: mpsc::Receiver<bytes::Bytes>) {
    tokio::spawn(async move {
        // A peer that stops reading while its connection stays up would
        // hold this task, and the frames queued for it, for good.
        while let Some(frame) = rx.recv().await {
            match tokio::time::timeout(Duration::from_secs(120), half.write_all(&frame)).await {
                Ok(Ok(())) => {}
                _ => break,
            }
        }
        let _ = half.shutdown().await;
    });
}

pub async fn connect(addr: std::net::SocketAddr, within: Duration) -> io::Result<TcpStream> {
    let stream = tokio::time::timeout(within, TcpStream::connect(addr))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    keepalive(&stream);
    Ok(stream)
}

/// Dead peers behind NAT never send a FIN; keepalive is what eventually
/// notices them without an application-level ping.
pub fn keepalive(stream: &TcpStream) {
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(20));
    let _ = socket2::SockRef::from(stream).set_tcp_keepalive(&ka);
}
