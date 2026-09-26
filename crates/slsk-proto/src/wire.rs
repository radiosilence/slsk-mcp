//! Packing primitives: little-endian integers, length-prefixed strings.
//!
//! Decoding never panics and never allocates more than the input can back: a
//! hostile count or length is checked against the bytes actually remaining
//! before anything is reserved.

use bytes::{BufMut, Bytes, BytesMut};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("message truncated: wanted {wanted} bytes at offset {at}, {left} left")]
    Truncated {
        at: usize,
        wanted: usize,
        left: usize,
    },
    #[error("count {0} exceeds what the message can hold")]
    Count(u32),
    #[error("zlib: {0}")]
    Zlib(String),
    #[error("inflated payload exceeds {0} bytes")]
    TooLarge(usize),
    #[error("{0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, DecodeError>;

/// A string as the peer sent it.
///
/// File paths travel as bytes and must go back byte-for-byte: a path a peer
/// sent in Latin-1 names nothing if it is decoded and re-encoded as UTF-8.
/// Everything else can be read lossily, but a path cannot.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct RawStr(pub Bytes);

impl RawStr {
    /// UTF-8 when it is, Latin-1 otherwise — the two encodings actually seen on
    /// the network. Latin-1 decodes every byte sequence, so this cannot fail.
    pub fn to_string_lossy(&self) -> String {
        match std::str::from_utf8(&self.0) {
            Ok(s) => s.to_owned(),
            Err(_) => self.0.iter().map(|&b| b as char).collect(),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<&str> for RawStr {
    fn from(s: &str) -> Self {
        Self(Bytes::copy_from_slice(s.as_bytes()))
    }
}

impl From<String> for RawStr {
    fn from(s: String) -> Self {
        Self(Bytes::from(s.into_bytes()))
    }
}

impl std::fmt::Debug for RawStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

impl std::fmt::Display for RawStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

pub struct Reader {
    buf: Bytes,
    pos: usize,
}

impl Reader {
    pub fn new(buf: Bytes) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<Bytes> {
        if n > self.remaining() {
            return Err(DecodeError::Truncated {
                at: self.pos,
                wanted: n,
                left: self.remaining(),
            });
        }
        let out = self.buf.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self.take(N)?;
        Ok(bytes[..].try_into().expect("take returned N bytes"))
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn bytes(&mut self) -> Result<Bytes> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub fn raw(&mut self) -> Result<RawStr> {
        Ok(RawStr(self.bytes()?))
    }

    pub fn string(&mut self) -> Result<String> {
        Ok(self.raw()?.to_string_lossy())
    }

    /// Everything not yet read.
    pub fn rest(&mut self) -> Bytes {
        let out = self.buf.slice(self.pos..);
        self.pos = self.buf.len();
        out
    }

    /// A count of items, each at least `min_item` bytes, checked against what
    /// is left so a forged count cannot make the caller reserve gigabytes.
    pub fn count(&mut self, min_item: usize) -> Result<usize> {
        let n = self.u32()?;
        if (n as usize).saturating_mul(min_item.max(1)) > self.remaining() {
            return Err(DecodeError::Count(n));
        }
        Ok(n as usize)
    }

    pub fn list<T>(
        &mut self,
        min_item: usize,
        mut item: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let n = self.count(min_item)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(item(self)?);
        }
        Ok(out)
    }

    pub fn strings(&mut self) -> Result<Vec<String>> {
        self.list(4, Self::string)
    }

    /// An IPv4 address as the protocol packs it: a little-endian u32 whose
    /// most significant byte is the first octet.
    pub fn ip(&mut self) -> Result<std::net::Ipv4Addr> {
        Ok(std::net::Ipv4Addr::from(self.u32()?))
    }
}

#[derive(Default)]
pub struct Writer(pub BytesMut);

impl Writer {
    pub fn new() -> Self {
        Self(BytesMut::with_capacity(64))
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.put_u8(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.put_u16_le(v);
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.put_u32_le(v);
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.0.put_i32_le(v);
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.put_u64_le(v);
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.put_slice(v);
        self
    }

    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }

    pub fn raw(&mut self, v: &RawStr) -> &mut Self {
        self.bytes(&v.0)
    }

    pub fn strings<S: AsRef<str>>(&mut self, v: &[S]) -> &mut Self {
        self.u32(v.len() as u32);
        for s in v {
            self.str(s.as_ref());
        }
        self
    }

    pub fn ip(&mut self, v: std::net::Ipv4Addr) -> &mut Self {
        self.u32(u32::from(v))
    }

    pub fn put(&mut self, v: &[u8]) -> &mut Self {
        self.0.put_slice(v);
        self
    }

    pub fn finish(self) -> Bytes {
        self.0.freeze()
    }
}

/// zlib-inflate at most `limit` bytes. Share lists run to tens of megabytes
/// for large collections, so the limit is generous but finite: a few hundred
/// bytes of hostile deflate stream can otherwise expand without bound.
pub fn inflate(data: &[u8], limit: usize) -> Result<Bytes> {
    use std::io::Read;
    let mut out = Vec::with_capacity((data.len() * 4).min(limit));
    let read = flate2::read::ZlibDecoder::new(data)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| DecodeError::Zlib(e.to_string()))?;
    if read > limit {
        return Err(DecodeError::TooLarge(limit));
    }
    Ok(Bytes::from(out))
}

pub fn deflate(data: &[u8]) -> Bytes {
    use std::io::Write;
    let mut enc = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(data.len() / 3),
        flate2::Compression::fast(),
    );
    enc.write_all(data).expect("writing to a Vec cannot fail");
    Bytes::from(enc.finish().expect("writing to a Vec cannot fail"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forged_count_is_refused_before_allocating() {
        let mut w = Writer::new();
        w.u32(u32::MAX);
        let mut r = Reader::new(w.finish());
        assert_eq!(r.list(4, Reader::u32), Err(DecodeError::Count(u32::MAX)));
    }

    #[test]
    fn latin1_paths_survive_a_round_trip() {
        let latin1 = RawStr(Bytes::from_static(b"Bj\xf6rk\\Hom\xe9genic"));
        assert_eq!(latin1.to_string_lossy(), "Björk\\Homégenic");
        let mut w = Writer::new();
        w.raw(&latin1);
        assert_eq!(Reader::new(w.finish()).raw().unwrap(), latin1);
    }

    #[test]
    fn ips_pack_first_octet_most_significant() {
        let mut w = Writer::new();
        w.ip("1.2.3.4".parse().unwrap());
        let bytes = w.finish();
        assert_eq!(&bytes[..], &[4, 3, 2, 1]);
        assert_eq!(Reader::new(bytes).ip().unwrap().to_string(), "1.2.3.4");
    }

    #[test]
    fn inflate_refuses_a_bomb() {
        let bomb = deflate(&vec![0u8; 1 << 20]);
        assert_eq!(inflate(&bomb, 1024), Err(DecodeError::TooLarge(1024)));
        assert_eq!(inflate(&bomb, 1 << 20).unwrap().len(), 1 << 20);
    }
}
