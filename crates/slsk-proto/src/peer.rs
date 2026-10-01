//! Peer-init (u8 codes), peer (u32 codes), file and distributed (u8) messages.

use bytes::Bytes;

use crate::ConnKind;
use crate::frame::{CodeWidth, encode};
use crate::wire::{DecodeError, RawStr, Reader, Result, Writer, deflate, inflate};

/// Largest inflated payload accepted from a peer. A browse of a very large
/// collection inflates to tens of megabytes; this leaves room for that and
/// refuses anything that could only be an attack.
pub const MAX_INFLATED: usize = 256 << 20;
/// For a search response or one folder's contents: thousands of files, a
/// few megabytes at most.
const MAX_INFLATED_PART: usize = 16 << 20;

/// The first message on any peer connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerInit {
    /// Answering an indirect connection request: the token from the server's
    /// ConnectToPeer.
    PierceFirewall { token: u32 },
    /// A direct connection. The token is always zero on today's network.
    PeerInit {
        username: String,
        kind: ConnKind,
        token: u32,
    },
}

impl PeerInit {
    pub fn encode(&self) -> Bytes {
        let mut w = Writer::new();
        match self {
            Self::PierceFirewall { token } => {
                w.u32(*token);
                encode(CodeWidth::U8, 0, &w.finish())
            }
            Self::PeerInit {
                username,
                kind,
                token,
            } => {
                w.str(username).str(kind.as_str()).u32(*token);
                encode(CodeWidth::U8, 1, &w.finish())
            }
        }
    }

    pub fn decode(code: u32, body: Bytes) -> Result<Self> {
        let mut r = Reader::new(body);
        match code {
            0 => Ok(Self::PierceFirewall { token: r.u32()? }),
            1 => Ok(Self::PeerInit {
                username: r.string()?,
                kind: ConnKind::parse(&r.string()?)
                    .ok_or(DecodeError::Invalid("connection type"))?,
                token: r.u32().unwrap_or(0),
            }),
            _ => Err(DecodeError::Invalid("peer init code")),
        }
    }
}

/// One file as searches, browses and folder listings describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: RawStr,
    pub size: u64,
    pub extension: String,
    pub attrs: Vec<(u32, u32)>,
}

impl FileEntry {
    pub const BITRATE: u32 = 0;
    pub const DURATION: u32 = 1;
    pub const VBR: u32 = 2;
    pub const SAMPLE_RATE: u32 = 4;
    pub const BIT_DEPTH: u32 = 5;

    pub fn attr(&self, code: u32) -> Option<u32> {
        self.attrs.iter().find(|(c, _)| *c == code).map(|(_, v)| *v)
    }

    fn read(r: &mut Reader) -> Result<Self> {
        let _code = r.u8()?;
        let name = r.raw()?;
        let size = r.u64()?;
        let extension = r.string()?;
        let attrs = r.list(8, |r| Ok((r.u32()?, r.u32()?)))?;
        Ok(Self {
            name,
            size,
            extension,
            attrs,
        })
    }

    pub fn write(&self, w: &mut Writer) {
        w.u8(1)
            .raw(&self.name)
            .u64(self.size)
            .str(&self.extension)
            .u32(self.attrs.len() as u32);
        for (c, v) in &self.attrs {
            w.u32(*c).u32(*v);
        }
    }
}

/// Minimum encoded size of a [`FileEntry`], for bounding counts.
const MIN_FILE: usize = 1 + 4 + 8 + 4 + 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directory {
    pub name: RawStr,
    pub files: Vec<FileEntry>,
}

impl Directory {
    fn read(r: &mut Reader) -> Result<Self> {
        Ok(Self {
            name: r.raw()?,
            files: r.list(MIN_FILE, FileEntry::read)?,
        })
    }

    pub fn write(&self, w: &mut Writer) {
        w.raw(&self.name).u32(self.files.len() as u32);
        for f in &self.files {
            f.write(w);
        }
    }
}

fn read_dirs(r: &mut Reader) -> Result<Vec<Directory>> {
    r.list(8, Directory::read)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResponse {
    pub username: String,
    pub token: u32,
    pub files: Vec<FileEntry>,
    pub slot_free: bool,
    pub avg_speed: u32,
    pub queue_length: u32,
    pub private_files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub description: String,
    pub picture: Option<Bytes>,
    pub total_uploads: u32,
    pub queue_size: u32,
    pub slots_free: bool,
    pub upload_permitted: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMessage {
    GetSharedFileList,
    SharedFileList {
        dirs: Vec<Directory>,
        private_dirs: Vec<Directory>,
    },
    SearchResponse(SearchResponse),
    UserInfoRequest,
    UserInfoResponse(UserInfo),
    FolderContentsRequest {
        token: u32,
        folder: RawStr,
    },
    FolderContentsResponse {
        token: u32,
        folder: RawStr,
        dirs: Vec<Directory>,
    },
    /// direction 0: a (legacy) download request; 1: the peer is ready to
    /// upload to us, and `size` is set.
    TransferRequest {
        direction: u32,
        token: u32,
        filename: RawStr,
        size: Option<u64>,
    },
    /// `size` only appears when accepting a legacy download request.
    TransferResponse {
        token: u32,
        allowed: bool,
        size: Option<u64>,
        reason: Option<String>,
    },
    QueueUpload {
        filename: RawStr,
    },
    PlaceInQueueResponse {
        filename: RawStr,
        place: u32,
    },
    UploadFailed {
        filename: RawStr,
    },
    UploadDenied {
        filename: RawStr,
        reason: String,
    },
    PlaceInQueueRequest {
        filename: RawStr,
    },
    Unknown {
        code: u32,
        body: Bytes,
    },
}

impl PeerMessage {
    pub fn code(&self) -> u32 {
        use PeerMessage::*;
        match self {
            GetSharedFileList => 4,
            SharedFileList { .. } => 5,
            SearchResponse(_) => 9,
            UserInfoRequest => 15,
            UserInfoResponse(_) => 16,
            FolderContentsRequest { .. } => 36,
            FolderContentsResponse { .. } => 37,
            TransferRequest { .. } => 40,
            TransferResponse { .. } => 41,
            QueueUpload { .. } => 43,
            PlaceInQueueResponse { .. } => 44,
            UploadFailed { .. } => 46,
            UploadDenied { .. } => 50,
            PlaceInQueueRequest { .. } => 51,
            Unknown { code, .. } => *code,
        }
    }

    /// The whole frame. Compressed messages are deflated here, so callers that
    /// send the same large payload repeatedly — a share list — should build it
    /// once with [`shared_file_list_frame`] and reuse the bytes.
    pub fn encode(&self) -> Bytes {
        use PeerMessage::*;
        let mut w = Writer::new();
        match self {
            GetSharedFileList | UserInfoRequest => {}
            SharedFileList { dirs, private_dirs } => {
                return shared_file_list_frame(dirs, private_dirs);
            }
            SearchResponse(s) => {
                let mut inner = Writer::new();
                inner
                    .str(&s.username)
                    .u32(s.token)
                    .u32(s.files.len() as u32);
                for f in &s.files {
                    f.write(&mut inner);
                }
                inner
                    .bool(s.slot_free)
                    .u32(s.avg_speed)
                    .u32(s.queue_length)
                    .u32(0)
                    .u32(s.private_files.len() as u32);
                for f in &s.private_files {
                    f.write(&mut inner);
                }
                w.put(&deflate(&inner.finish()));
            }
            UserInfoResponse(u) => {
                w.str(&u.description);
                match &u.picture {
                    Some(p) => {
                        w.bool(true).bytes(p);
                    }
                    None => {
                        w.bool(false);
                    }
                }
                w.u32(u.total_uploads).u32(u.queue_size).bool(u.slots_free);
                if let Some(p) = u.upload_permitted {
                    w.u32(p);
                }
            }
            FolderContentsRequest { token, folder } => {
                w.u32(*token).raw(folder);
            }
            FolderContentsResponse {
                token,
                folder,
                dirs,
            } => {
                let mut inner = Writer::new();
                inner.u32(*token).raw(folder).u32(dirs.len() as u32);
                for d in dirs {
                    d.write(&mut inner);
                }
                w.put(&deflate(&inner.finish()));
            }
            TransferRequest {
                direction,
                token,
                filename,
                size,
            } => {
                w.u32(*direction).u32(*token).raw(filename);
                if let (1, Some(size)) = (direction, size) {
                    w.u64(*size);
                }
            }
            TransferResponse {
                token,
                allowed,
                size,
                reason,
            } => {
                w.u32(*token).bool(*allowed);
                if *allowed {
                    if let Some(size) = size {
                        w.u64(*size);
                    }
                } else {
                    w.str(reason.as_deref().unwrap_or("Cancelled"));
                }
            }
            QueueUpload { filename }
            | UploadFailed { filename }
            | PlaceInQueueRequest { filename } => {
                w.raw(filename);
            }
            PlaceInQueueResponse { filename, place } => {
                w.raw(filename).u32(*place);
            }
            UploadDenied { filename, reason } => {
                w.raw(filename).str(reason);
            }
            Unknown { body, .. } => {
                w.put(body);
            }
        }
        encode(CodeWidth::U32, self.code(), &w.finish())
    }

    pub fn decode(code: u32, body: Bytes) -> Result<Self> {
        use PeerMessage as P;
        let mut outer = Reader::new(body.clone());
        Ok(match code {
            4 => P::GetSharedFileList,
            5 => {
                let mut r = Reader::new(inflate(&outer.rest(), MAX_INFLATED)?);
                let dirs = read_dirs(&mut r)?;
                // Older clients stop after the public list.
                let private_dirs = if r.remaining() >= 8 {
                    let _unknown = r.u32()?;
                    read_dirs(&mut r)?
                } else {
                    Vec::new()
                };
                P::SharedFileList { dirs, private_dirs }
            }
            9 => {
                let mut r = Reader::new(inflate(&outer.rest(), MAX_INFLATED_PART)?);
                let username = r.string()?;
                let token = r.u32()?;
                let files = r.list(MIN_FILE, FileEntry::read)?;
                let slot_free = r.bool()?;
                let avg_speed = r.u32()?;
                let queue_length = r.u32()?;
                let private_files = if r.remaining() >= 8 {
                    let _unknown = r.u32()?;
                    r.list(MIN_FILE, FileEntry::read)?
                } else {
                    Vec::new()
                };
                P::SearchResponse(SearchResponse {
                    username,
                    token,
                    files,
                    slot_free,
                    avg_speed,
                    queue_length,
                    private_files,
                })
            }
            15 => P::UserInfoRequest,
            16 => {
                let r = &mut outer;
                let description = r.string()?;
                let picture = if r.bool()? { Some(r.bytes()?) } else { None };
                P::UserInfoResponse(UserInfo {
                    description,
                    picture,
                    total_uploads: r.u32()?,
                    queue_size: r.u32()?,
                    slots_free: r.bool()?,
                    upload_permitted: r.u32().ok(),
                })
            }
            36 => P::FolderContentsRequest {
                token: outer.u32()?,
                folder: outer.raw()?,
            },
            37 => {
                let mut r = Reader::new(inflate(&outer.rest(), MAX_INFLATED_PART)?);
                P::FolderContentsResponse {
                    token: r.u32()?,
                    folder: r.raw()?,
                    dirs: read_dirs(&mut r)?,
                }
            }
            40 => {
                let r = &mut outer;
                let direction = r.u32()?;
                let token = r.u32()?;
                let filename = r.raw()?;
                let size = if direction == 1 { Some(r.u64()?) } else { None };
                P::TransferRequest {
                    direction,
                    token,
                    filename,
                    size,
                }
            }
            41 => {
                let r = &mut outer;
                let token = r.u32()?;
                let allowed = r.bool()?;
                if allowed {
                    P::TransferResponse {
                        token,
                        allowed,
                        size: r.u64().ok(),
                        reason: None,
                    }
                } else {
                    P::TransferResponse {
                        token,
                        allowed,
                        size: None,
                        reason: r.string().ok(),
                    }
                }
            }
            43 => P::QueueUpload {
                filename: outer.raw()?,
            },
            44 => P::PlaceInQueueResponse {
                filename: outer.raw()?,
                place: outer.u32()?,
            },
            46 => P::UploadFailed {
                filename: outer.raw()?,
            },
            50 => P::UploadDenied {
                filename: outer.raw()?,
                reason: outer.string()?,
            },
            51 => P::PlaceInQueueRequest {
                filename: outer.raw()?,
            },
            _ => P::Unknown { code, body },
        })
    }
}

/// A complete, compressed SharedFileListResponse frame.
///
/// Built once per share rescan and reused for every browse: compressing a
/// large collection costs real CPU, and it is the same bytes every time.
pub fn shared_file_list_frame(dirs: &[Directory], private_dirs: &[Directory]) -> Bytes {
    let mut inner = Writer::new();
    inner.u32(dirs.len() as u32);
    for d in dirs {
        d.write(&mut inner);
    }
    inner.u32(0).u32(private_dirs.len() as u32);
    for d in private_dirs {
        d.write(&mut inner);
    }
    encode(CodeWidth::U32, 5, &deflate(&inner.finish()))
}

/// File connections: after the peer init, the uploader sends the transfer
/// token and the downloader answers with the offset to resume from. Neither
/// is framed.
pub fn file_transfer_init(token: u32) -> [u8; 4] {
    token.to_le_bytes()
}

pub fn file_offset(offset: u64) -> [u8; 8] {
    offset.to_le_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistribMessage {
    Ping,
    /// The raw body is kept so it can be forwarded to children untouched,
    /// including anything a newer client appended.
    Search {
        username: String,
        token: u32,
        query: String,
        raw: Bytes,
    },
    BranchLevel {
        level: i32,
    },
    BranchRoot {
        root: String,
    },
    ChildDepth {
        depth: u32,
    },
    Embedded {
        code: u8,
        payload: Bytes,
    },
    Unknown {
        code: u32,
        body: Bytes,
    },
}

impl DistribMessage {
    pub fn decode(code: u32, body: Bytes) -> Result<Self> {
        let mut r = Reader::new(body.clone());
        Ok(match code {
            0 => Self::Ping,
            3 => {
                if r.u32()? != 49 {
                    return Err(DecodeError::Invalid("distributed search identifier"));
                }
                Self::Search {
                    username: r.string()?,
                    token: r.u32()?,
                    query: r.string()?,
                    raw: body,
                }
            }
            4 => Self::BranchLevel { level: r.i32()? },
            5 => Self::BranchRoot { root: r.string()? },
            7 => Self::ChildDepth { depth: r.u32()? },
            93 => Self::Embedded {
                code: r.u8()?,
                payload: r.rest(),
            },
            _ => Self::Unknown { code, body },
        })
    }

    pub fn encode(&self) -> Bytes {
        let mut w = Writer::new();
        let code = match self {
            Self::Ping => 0,
            Self::Search { raw, .. } => return encode(CodeWidth::U8, 3, raw),
            Self::BranchLevel { level } => {
                w.i32(*level);
                4
            }
            Self::BranchRoot { root } => {
                w.str(root);
                5
            }
            Self::ChildDepth { depth } => {
                w.u32(*depth);
                7
            }
            Self::Embedded { code, payload } => {
                w.u8(*code).put(payload);
                93
            }
            Self::Unknown { code, body } => return encode(CodeWidth::U8, *code, body),
        };
        encode(CodeWidth::U8, code, &w.finish())
    }

    /// A search frame from its parts, for when we are the branch root and
    /// the server handed us the body.
    pub fn search_frame(raw: &Bytes) -> Bytes {
        encode(CodeWidth::U8, 3, raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::decode;
    use bytes::BytesMut;

    fn peer_roundtrip(m: &PeerMessage) -> PeerMessage {
        let frame = m.encode();
        let mut buf = BytesMut::from(&frame[..]);
        let f = decode(&mut buf, CodeWidth::U32, 1 << 24).unwrap().unwrap();
        PeerMessage::decode(f.code, f.body).unwrap()
    }

    fn file(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            size: 1234,
            extension: "flac".into(),
            attrs: vec![(1, 200), (4, 44100), (5, 16)],
        }
    }

    #[test]
    fn peer_messages_round_trip() {
        let dirs = vec![Directory {
            name: "music\\A".into(),
            files: vec![file("01.flac"), file("02.flac")],
        }];
        for m in [
            PeerMessage::GetSharedFileList,
            PeerMessage::SharedFileList {
                dirs: dirs.clone(),
                private_dirs: vec![],
            },
            PeerMessage::SearchResponse(SearchResponse {
                username: "me".into(),
                token: 7,
                files: vec![file("music\\A\\01.flac")],
                slot_free: true,
                avg_speed: 1000,
                queue_length: 3,
                private_files: vec![],
            }),
            PeerMessage::UserInfoResponse(UserInfo {
                description: "hi".into(),
                picture: None,
                total_uploads: 5,
                queue_size: 1,
                slots_free: true,
                upload_permitted: Some(1),
            }),
            PeerMessage::FolderContentsResponse {
                token: 3,
                folder: "music\\A".into(),
                dirs,
            },
            PeerMessage::TransferRequest {
                direction: 1,
                token: 9,
                filename: "x".into(),
                size: Some(10),
            },
            PeerMessage::TransferRequest {
                direction: 0,
                token: 9,
                filename: "x".into(),
                size: None,
            },
            PeerMessage::TransferResponse {
                token: 9,
                allowed: false,
                size: None,
                reason: Some("Queued".into()),
            },
            PeerMessage::TransferResponse {
                token: 9,
                allowed: true,
                size: None,
                reason: None,
            },
            PeerMessage::QueueUpload {
                filename: "x".into(),
            },
            PeerMessage::PlaceInQueueResponse {
                filename: "x".into(),
                place: 4,
            },
            PeerMessage::UploadDenied {
                filename: "x".into(),
                reason: "Banned".into(),
            },
        ] {
            assert_eq!(peer_roundtrip(&m), m);
        }
    }

    #[test]
    fn distributed_search_forwards_its_raw_body() {
        let mut w = Writer::new();
        w.u32(49).str("alice").u32(5).str("aphex twin").u32(123);
        let raw = w.finish();
        let frame = DistribMessage::search_frame(&raw);
        let mut buf = BytesMut::from(&frame[..]);
        let f = decode(&mut buf, CodeWidth::U8, 1 << 16).unwrap().unwrap();
        let m = DistribMessage::decode(f.code, f.body).unwrap();
        match &m {
            DistribMessage::Search {
                username, query, ..
            } => assert_eq!((username.as_str(), query.as_str()), ("alice", "aphex twin")),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            m.encode(),
            frame,
            "the trailing field must survive forwarding"
        );
    }

    #[test]
    fn peer_init_round_trips() {
        for m in [
            PeerInit::PierceFirewall { token: 5 },
            PeerInit::PeerInit {
                username: "bob".into(),
                kind: ConnKind::Distributed,
                token: 0,
            },
        ] {
            let frame = m.encode();
            let mut buf = BytesMut::from(&frame[..]);
            let f = decode(&mut buf, CodeWidth::U8, 1024).unwrap().unwrap();
            assert_eq!(PeerInit::decode(f.code, f.body).unwrap(), m);
        }
    }
}
