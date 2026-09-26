//! Server messages (u32 codes), both directions.
//!
//! Every message the modern network still speaks. Obsolete codes the server no
//! longer sends are not modelled; anything unrecognised decodes to
//! [`FromServer::Unknown`] rather than failing the connection.

use std::net::Ipv4Addr;

use bytes::Bytes;

use crate::ConnKind;
use crate::frame::{CodeWidth, encode};
use crate::wire::{Reader, Result, Writer};

/// The major version this client logs in with. 177 is the one the protocol
/// documentation sets aside for experimental clients, until a number of our
/// own is reserved.
pub const MAJOR_VERSION: u32 = 177;
pub const MINOR_VERSION: u32 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserStatus {
    #[default]
    Offline,
    Away,
    Online,
}

impl UserStatus {
    pub fn from_code(code: u32) -> Self {
        match code {
            1 => Self::Away,
            2 => Self::Online,
            _ => Self::Offline,
        }
    }

    pub fn code(self) -> u32 {
        match self {
            Self::Offline => 0,
            Self::Away => 1,
            Self::Online => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UserStats {
    pub avg_speed: u32,
    pub upload_num: u32,
    pub files: u32,
    pub dirs: u32,
}

impl UserStats {
    fn read(r: &mut Reader) -> Result<Self> {
        let avg_speed = r.u32()?;
        let upload_num = r.u32()?;
        let _unknown = r.u32()?;
        Ok(Self {
            avg_speed,
            upload_num,
            files: r.u32()?,
            dirs: r.u32()?,
        })
    }

    fn write(&self, w: &mut Writer) {
        w.u32(self.avg_speed)
            .u32(self.upload_num)
            .u32(0)
            .u32(self.files)
            .u32(self.dirs);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomUser {
    pub username: String,
    pub status: UserStatus,
    pub stats: UserStats,
    pub slots_full: bool,
    pub country: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub item: String,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomList {
    pub public: Vec<(String, u32)>,
    pub owned_private: Vec<(String, u32)>,
    pub private: Vec<(String, u32)>,
    pub operated_private: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PossibleParent {
    pub username: String,
    pub ip: Ipv4Addr,
    pub port: u32,
}

/// What we send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToServer {
    Login {
        username: String,
        password: String,
    },
    SetWaitPort {
        port: u32,
    },
    GetPeerAddress {
        username: String,
    },
    WatchUser {
        username: String,
    },
    UnwatchUser {
        username: String,
    },
    GetUserStatus {
        username: String,
    },
    SayChatroom {
        room: String,
        message: String,
    },
    JoinRoom {
        room: String,
        private: bool,
    },
    LeaveRoom {
        room: String,
    },
    ConnectToPeer {
        token: u32,
        username: String,
        kind: ConnKind,
    },
    MessageUser {
        username: String,
        message: String,
    },
    MessageAcked {
        id: u32,
    },
    FileSearch {
        token: u32,
        query: String,
    },
    SetStatus {
        status: UserStatus,
    },
    ServerPing,
    SharedFoldersFiles {
        dirs: u32,
        files: u32,
    },
    GetUserStats {
        username: String,
    },
    UserSearch {
        username: String,
        token: u32,
        query: String,
    },
    AddThingILike {
        item: String,
    },
    RemoveThingILike {
        item: String,
    },
    AddThingIHate {
        item: String,
    },
    RemoveThingIHate {
        item: String,
    },
    Recommendations,
    GlobalRecommendations,
    UserInterests {
        username: String,
    },
    RoomList,
    CheckPrivileges,
    HaveNoParent {
        no_parent: bool,
    },
    AcceptChildren {
        accept: bool,
    },
    WishlistSearch {
        token: u32,
        query: String,
    },
    SimilarUsers,
    ItemRecommendations {
        item: String,
    },
    ItemSimilarUsers {
        item: String,
    },
    SetRoomTicker {
        room: String,
        ticker: String,
    },
    RoomSearch {
        room: String,
        token: u32,
        query: String,
    },
    SendUploadSpeed {
        speed: u32,
    },
    GivePrivileges {
        username: String,
        days: u32,
    },
    BranchLevel {
        level: u32,
    },
    BranchRoot {
        root: String,
    },
    AddRoomMember {
        room: String,
        username: String,
    },
    RemoveRoomMember {
        room: String,
        username: String,
    },
    CancelRoomMembership {
        room: String,
    },
    CancelRoomOwnership {
        room: String,
    },
    EnableRoomInvitations {
        enable: bool,
    },
    ChangePassword {
        password: String,
    },
    AddRoomOperator {
        room: String,
        username: String,
    },
    RemoveRoomOperator {
        room: String,
        username: String,
    },
    MessageUsers {
        usernames: Vec<String>,
        message: String,
    },
    JoinGlobalRoom,
    LeaveGlobalRoom,
    CantConnectToPeer {
        token: u32,
        username: String,
    },
}

impl ToServer {
    pub fn code(&self) -> u32 {
        use ToServer::*;
        match self {
            Login { .. } => 1,
            SetWaitPort { .. } => 2,
            GetPeerAddress { .. } => 3,
            WatchUser { .. } => 5,
            UnwatchUser { .. } => 6,
            GetUserStatus { .. } => 7,
            SayChatroom { .. } => 13,
            JoinRoom { .. } => 14,
            LeaveRoom { .. } => 15,
            ConnectToPeer { .. } => 18,
            MessageUser { .. } => 22,
            MessageAcked { .. } => 23,
            FileSearch { .. } => 26,
            SetStatus { .. } => 28,
            ServerPing => 32,
            SharedFoldersFiles { .. } => 35,
            GetUserStats { .. } => 36,
            UserSearch { .. } => 42,
            AddThingILike { .. } => 51,
            RemoveThingILike { .. } => 52,
            Recommendations => 54,
            GlobalRecommendations => 56,
            UserInterests { .. } => 57,
            RoomList => 64,
            CheckPrivileges => 92,
            HaveNoParent { .. } => 71,
            AcceptChildren { .. } => 100,
            WishlistSearch { .. } => 103,
            SimilarUsers => 110,
            ItemRecommendations { .. } => 111,
            ItemSimilarUsers { .. } => 112,
            SetRoomTicker { .. } => 116,
            AddThingIHate { .. } => 117,
            RemoveThingIHate { .. } => 118,
            RoomSearch { .. } => 120,
            SendUploadSpeed { .. } => 121,
            GivePrivileges { .. } => 123,
            BranchLevel { .. } => 126,
            BranchRoot { .. } => 127,
            AddRoomMember { .. } => 134,
            RemoveRoomMember { .. } => 135,
            CancelRoomMembership { .. } => 136,
            CancelRoomOwnership { .. } => 137,
            EnableRoomInvitations { .. } => 141,
            ChangePassword { .. } => 142,
            AddRoomOperator { .. } => 143,
            RemoveRoomOperator { .. } => 144,
            MessageUsers { .. } => 149,
            JoinGlobalRoom => 150,
            LeaveGlobalRoom => 151,
            CantConnectToPeer { .. } => 1001,
        }
    }

    /// The whole frame, ready for the socket.
    pub fn encode(&self) -> Bytes {
        use ToServer::*;
        let mut w = Writer::new();
        match self {
            Login { username, password } => {
                let digest = <md5::Md5 as md5::Digest>::digest(format!("{username}{password}"));
                let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                w.str(username)
                    .str(password)
                    .u32(MAJOR_VERSION)
                    .str(&hash)
                    .u32(MINOR_VERSION);
            }
            SetWaitPort { port } => {
                w.u32(*port);
            }
            GetPeerAddress { username }
            | WatchUser { username }
            | UnwatchUser { username }
            | GetUserStatus { username }
            | GetUserStats { username }
            | UserInterests { username } => {
                w.str(username);
            }
            SayChatroom { room, message } => {
                w.str(room).str(message);
            }
            JoinRoom { room, private } => {
                w.str(room).u32(*private as u32);
            }
            LeaveRoom { room } | CancelRoomMembership { room } | CancelRoomOwnership { room } => {
                w.str(room);
            }
            ConnectToPeer {
                token,
                username,
                kind,
            } => {
                w.u32(*token).str(username).str(kind.as_str());
            }
            MessageUser { username, message } => {
                w.str(username).str(message);
            }
            MessageAcked { id } => {
                w.u32(*id);
            }
            FileSearch { token, query } | WishlistSearch { token, query } => {
                w.u32(*token).str(query);
            }
            SetStatus { status } => {
                w.i32(status.code() as i32);
            }
            ServerPing
            | Recommendations
            | GlobalRecommendations
            | RoomList
            | CheckPrivileges
            | SimilarUsers
            | JoinGlobalRoom
            | LeaveGlobalRoom => {}
            SharedFoldersFiles { dirs, files } => {
                w.u32(*dirs).u32(*files);
            }
            UserSearch {
                username,
                token,
                query,
            } => {
                w.str(username).u32(*token).str(query);
            }
            AddThingILike { item }
            | RemoveThingILike { item }
            | AddThingIHate { item }
            | RemoveThingIHate { item }
            | ItemRecommendations { item }
            | ItemSimilarUsers { item } => {
                w.str(item);
            }
            HaveNoParent { no_parent } => {
                w.bool(*no_parent);
            }
            AcceptChildren { accept } => {
                w.bool(*accept);
            }
            SetRoomTicker { room, ticker } => {
                w.str(room).str(ticker);
            }
            RoomSearch { room, token, query } => {
                w.str(room).u32(*token).str(query);
            }
            SendUploadSpeed { speed } => {
                w.u32(*speed);
            }
            GivePrivileges { username, days } => {
                w.str(username).u32(*days);
            }
            BranchLevel { level } => {
                w.u32(*level);
            }
            BranchRoot { root } => {
                w.str(root);
            }
            AddRoomMember { room, username }
            | RemoveRoomMember { room, username }
            | AddRoomOperator { room, username }
            | RemoveRoomOperator { room, username } => {
                w.str(room).str(username);
            }
            EnableRoomInvitations { enable } => {
                w.bool(*enable);
            }
            ChangePassword { password } => {
                w.str(password);
            }
            MessageUsers { usernames, message } => {
                w.strings(usernames).str(message);
            }
            CantConnectToPeer { token, username } => {
                w.u32(*token).str(username);
            }
        }
        encode(CodeWidth::U32, self.code(), &w.finish())
    }
}

/// What the server sends us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromServer {
    LoginOk {
        greeting: String,
        own_ip: Ipv4Addr,
        supporter: bool,
    },
    LoginRejected {
        reason: String,
        detail: Option<String>,
    },
    PeerAddress {
        username: String,
        ip: Ipv4Addr,
        port: u32,
        obfuscated_port: Option<u32>,
    },
    WatchUser {
        username: String,
        exists: bool,
        status: UserStatus,
        stats: UserStats,
        country: Option<String>,
    },
    UserStatus {
        username: String,
        status: UserStatus,
        privileged: bool,
    },
    SayChatroom {
        room: String,
        username: String,
        message: String,
    },
    JoinRoom {
        room: String,
        users: Vec<RoomUser>,
        owner: Option<String>,
        operators: Vec<String>,
    },
    LeaveRoom {
        room: String,
    },
    UserJoinedRoom {
        room: String,
        user: RoomUser,
    },
    UserLeftRoom {
        room: String,
        username: String,
    },
    ConnectToPeer {
        username: String,
        kind: ConnKind,
        ip: Ipv4Addr,
        port: u32,
        token: u32,
        privileged: bool,
    },
    MessageUser {
        id: u32,
        timestamp: u32,
        username: String,
        message: String,
        is_new: bool,
    },
    FileSearch {
        username: String,
        token: u32,
        query: String,
    },
    UserStats {
        username: String,
        stats: UserStats,
    },
    Relogged,
    Recommendations {
        recommendations: Vec<Recommendation>,
        unrecommendations: Vec<Recommendation>,
    },
    GlobalRecommendations {
        recommendations: Vec<Recommendation>,
        unrecommendations: Vec<Recommendation>,
    },
    UserInterests {
        username: String,
        likes: Vec<String>,
        hates: Vec<String>,
    },
    RoomList(RoomList),
    AdminMessage {
        message: String,
    },
    PrivilegedUsers {
        usernames: Vec<String>,
    },
    ParentMinSpeed {
        speed: u32,
    },
    ParentSpeedRatio {
        ratio: u32,
    },
    CheckPrivileges {
        seconds_left: u32,
    },
    /// A distributed message for us, as a branch root, to hand to children.
    EmbeddedMessage {
        code: u8,
        payload: Bytes,
    },
    PossibleParents {
        parents: Vec<PossibleParent>,
    },
    WishlistInterval {
        seconds: u32,
    },
    SimilarUsers {
        users: Vec<(String, u32)>,
    },
    ItemRecommendations {
        item: String,
        recommendations: Vec<Recommendation>,
    },
    ItemSimilarUsers {
        item: String,
        usernames: Vec<String>,
    },
    RoomTickers {
        room: String,
        tickers: Vec<(String, String)>,
    },
    RoomTickerAdded {
        room: String,
        username: String,
        ticker: String,
    },
    RoomTickerRemoved {
        room: String,
        username: String,
    },
    ResetDistributed,
    RoomMembers {
        room: String,
        members: Vec<String>,
    },
    AddRoomMember {
        room: String,
        username: String,
    },
    RemoveRoomMember {
        room: String,
        username: String,
    },
    RoomMembershipGranted {
        room: String,
    },
    RoomMembershipRevoked {
        room: String,
    },
    EnableRoomInvitations {
        enabled: bool,
    },
    ChangePassword {
        password: String,
    },
    AddRoomOperator {
        room: String,
        username: String,
    },
    RemoveRoomOperator {
        room: String,
        username: String,
    },
    RoomOperatorshipGranted {
        room: String,
    },
    RoomOperatorshipRevoked {
        room: String,
    },
    RoomOperators {
        room: String,
        operators: Vec<String>,
    },
    GlobalRoomMessage {
        room: String,
        username: String,
        message: String,
    },
    ExcludedSearchPhrases {
        phrases: Vec<String>,
    },
    CantConnectToPeer {
        token: u32,
    },
    CantCreateRoom {
        room: String,
    },
    Unknown {
        code: u32,
        body: Bytes,
    },
}

fn recommendations(r: &mut Reader) -> Result<Vec<Recommendation>> {
    r.list(8, |r| {
        Ok(Recommendation {
            item: r.string()?,
            score: r.i32()?,
        })
    })
}

impl FromServer {
    pub fn decode(code: u32, body: Bytes) -> Result<Self> {
        use FromServer as F;
        let mut r = Reader::new(body.clone());
        let r = &mut r;
        Ok(match code {
            1 => {
                if r.bool()? {
                    let greeting = r.string()?;
                    let own_ip = r.ip()?;
                    let _password_hash = r.string()?;
                    let supporter = r.bool().unwrap_or(false);
                    F::LoginOk {
                        greeting,
                        own_ip,
                        supporter,
                    }
                } else {
                    let reason = r.string()?;
                    let detail = if r.is_empty() {
                        None
                    } else {
                        Some(r.string()?)
                    };
                    F::LoginRejected { reason, detail }
                }
            }
            3 => {
                let username = r.string()?;
                let ip = r.ip()?;
                let port = r.u32()?;
                let obfuscated_port = match (r.u32(), r.u16()) {
                    (Ok(1), Ok(p)) if p != 0 => Some(u32::from(p)),
                    _ => None,
                };
                F::PeerAddress {
                    username,
                    ip,
                    port,
                    obfuscated_port,
                }
            }
            5 => {
                let username = r.string()?;
                let exists = r.bool()?;
                if !exists {
                    F::WatchUser {
                        username,
                        exists,
                        status: UserStatus::Offline,
                        stats: UserStats::default(),
                        country: None,
                    }
                } else {
                    let status = UserStatus::from_code(r.u32()?);
                    let stats = UserStats::read(r)?;
                    let country = if status == UserStatus::Offline || r.is_empty() {
                        None
                    } else {
                        Some(r.string()?)
                    };
                    F::WatchUser {
                        username,
                        exists,
                        status,
                        stats,
                        country,
                    }
                }
            }
            7 => F::UserStatus {
                username: r.string()?,
                status: UserStatus::from_code(r.u32()?),
                privileged: r.bool().unwrap_or(false),
            },
            13 => F::SayChatroom {
                room: r.string()?,
                username: r.string()?,
                message: r.string()?,
            },
            14 => {
                let room = r.string()?;
                let names = r.strings()?;
                let statuses = r.list(4, Reader::u32)?;
                let stats = r.list(20, UserStats::read)?;
                let slots = r.list(4, Reader::u32)?;
                let countries = if r.is_empty() {
                    Vec::new()
                } else {
                    r.strings()?
                };
                let (owner, operators) = if r.is_empty() {
                    (None, Vec::new())
                } else {
                    (Some(r.string()?), r.strings()?)
                };
                let users = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, username)| RoomUser {
                        username,
                        status: UserStatus::from_code(statuses.get(i).copied().unwrap_or(0)),
                        stats: stats.get(i).copied().unwrap_or_default(),
                        slots_full: slots.get(i).copied().unwrap_or(0) != 0,
                        country: countries.get(i).cloned().filter(|c| !c.is_empty()),
                    })
                    .collect();
                F::JoinRoom {
                    room,
                    users,
                    owner,
                    operators,
                }
            }
            15 => F::LeaveRoom { room: r.string()? },
            16 => {
                let room = r.string()?;
                let username = r.string()?;
                let status = UserStatus::from_code(r.u32()?);
                let stats = UserStats::read(r)?;
                let slots_full = r.u32()? != 0;
                let country = r.string().ok().filter(|c| !c.is_empty());
                F::UserJoinedRoom {
                    room,
                    user: RoomUser {
                        username,
                        status,
                        stats,
                        slots_full,
                        country,
                    },
                }
            }
            17 => F::UserLeftRoom {
                room: r.string()?,
                username: r.string()?,
            },
            18 => F::ConnectToPeer {
                username: r.string()?,
                kind: ConnKind::parse(&r.string()?)
                    .ok_or(crate::wire::DecodeError::Invalid("connection type"))?,
                ip: r.ip()?,
                port: r.u32()?,
                token: r.u32()?,
                privileged: r.bool().unwrap_or(false),
            },
            22 => F::MessageUser {
                id: r.u32()?,
                timestamp: r.u32()?,
                username: r.string()?,
                message: r.string()?,
                is_new: r.bool().unwrap_or(true),
            },
            26 => F::FileSearch {
                username: r.string()?,
                token: r.u32()?,
                query: r.string()?,
            },
            36 => F::UserStats {
                username: r.string()?,
                stats: UserStats::read(r)?,
            },
            41 => F::Relogged,
            54 | 56 => {
                let recommendations = recommendations(r)?;
                let unrecommendations = if r.is_empty() {
                    Vec::new()
                } else {
                    recommendations_tail(r)?
                };
                if code == 54 {
                    F::Recommendations {
                        recommendations,
                        unrecommendations,
                    }
                } else {
                    F::GlobalRecommendations {
                        recommendations,
                        unrecommendations,
                    }
                }
            }
            57 => F::UserInterests {
                username: r.string()?,
                likes: r.strings()?,
                hates: r.strings()?,
            },
            64 => {
                let names = r.strings()?;
                let counts = r.list(4, Reader::u32)?;
                let owned = r.strings()?;
                let owned_counts = r.list(4, Reader::u32)?;
                let private = r.strings()?;
                let private_counts = r.list(4, Reader::u32)?;
                let operated_private = if r.is_empty() {
                    Vec::new()
                } else {
                    r.strings()?
                };
                let zip = |n: Vec<String>, c: Vec<u32>| {
                    n.into_iter()
                        .zip(c.into_iter().chain(std::iter::repeat(0)))
                        .collect()
                };
                F::RoomList(RoomList {
                    public: zip(names, counts),
                    owned_private: zip(owned, owned_counts),
                    private: zip(private, private_counts),
                    operated_private,
                })
            }
            66 => F::AdminMessage {
                message: r.string()?,
            },
            69 => F::PrivilegedUsers {
                usernames: r.strings()?,
            },
            83 => F::ParentMinSpeed { speed: r.u32()? },
            84 => F::ParentSpeedRatio { ratio: r.u32()? },
            92 => F::CheckPrivileges {
                seconds_left: r.u32()?,
            },
            93 => F::EmbeddedMessage {
                code: r.u8()?,
                payload: r.rest(),
            },
            102 => F::PossibleParents {
                parents: r.list(12, |r| {
                    Ok(PossibleParent {
                        username: r.string()?,
                        ip: r.ip()?,
                        port: r.u32()?,
                    })
                })?,
            },
            104 => F::WishlistInterval { seconds: r.u32()? },
            110 => F::SimilarUsers {
                users: r.list(8, |r| Ok((r.string()?, r.u32()?)))?,
            },
            111 => F::ItemRecommendations {
                item: r.string()?,
                recommendations: recommendations(r)?,
            },
            112 => F::ItemSimilarUsers {
                item: r.string()?,
                usernames: r.strings()?,
            },
            113 => F::RoomTickers {
                room: r.string()?,
                tickers: r.list(8, |r| Ok((r.string()?, r.string()?)))?,
            },
            114 => F::RoomTickerAdded {
                room: r.string()?,
                username: r.string()?,
                ticker: r.string()?,
            },
            115 => F::RoomTickerRemoved {
                room: r.string()?,
                username: r.string()?,
            },
            130 => F::ResetDistributed,
            133 => F::RoomMembers {
                room: r.string()?,
                members: r.strings()?,
            },
            134 => F::AddRoomMember {
                room: r.string()?,
                username: r.string()?,
            },
            135 => F::RemoveRoomMember {
                room: r.string()?,
                username: r.string()?,
            },
            139 => F::RoomMembershipGranted { room: r.string()? },
            140 => F::RoomMembershipRevoked { room: r.string()? },
            141 => F::EnableRoomInvitations { enabled: r.bool()? },
            142 => F::ChangePassword {
                password: r.string()?,
            },
            143 => F::AddRoomOperator {
                room: r.string()?,
                username: r.string()?,
            },
            144 => F::RemoveRoomOperator {
                room: r.string()?,
                username: r.string()?,
            },
            145 => F::RoomOperatorshipGranted { room: r.string()? },
            146 => F::RoomOperatorshipRevoked { room: r.string()? },
            148 => F::RoomOperators {
                room: r.string()?,
                operators: r.strings()?,
            },
            152 => F::GlobalRoomMessage {
                room: r.string()?,
                username: r.string()?,
                message: r.string()?,
            },
            160 => F::ExcludedSearchPhrases {
                phrases: r.strings()?,
            },
            1001 => F::CantConnectToPeer { token: r.u32()? },
            1003 => F::CantCreateRoom { room: r.string()? },
            _ => F::Unknown { code, body },
        })
    }
}

fn recommendations_tail(r: &mut Reader) -> Result<Vec<Recommendation>> {
    recommendations(r)
}

/// Test support: the server side of the wire, so a mock server can be written
/// with the same types.
pub mod mock {
    use super::*;

    pub fn login_ok(greeting: &str, ip: Ipv4Addr) -> Bytes {
        let mut w = Writer::new();
        w.bool(true).str(greeting).ip(ip).str("").bool(false);
        encode(CodeWidth::U32, 1, &w.finish())
    }

    pub fn peer_address(username: &str, ip: Ipv4Addr, port: u32) -> Bytes {
        let mut w = Writer::new();
        w.str(username).ip(ip).u32(port).u32(0).u16(0);
        encode(CodeWidth::U32, 3, &w.finish())
    }

    pub fn connect_to_peer(
        username: &str,
        kind: ConnKind,
        ip: Ipv4Addr,
        port: u32,
        token: u32,
    ) -> Bytes {
        let mut w = Writer::new();
        w.str(username)
            .str(kind.as_str())
            .ip(ip)
            .u32(port)
            .u32(token)
            .bool(false)
            .u32(0)
            .u32(0);
        encode(CodeWidth::U32, 18, &w.finish())
    }

    pub fn file_search(username: &str, token: u32, query: &str) -> Bytes {
        let mut w = Writer::new();
        w.str(username).u32(token).str(query);
        encode(CodeWidth::U32, 26, &w.finish())
    }

    pub fn user_stats(username: &str, stats: UserStats) -> Bytes {
        let mut w = Writer::new();
        w.str(username);
        stats.write(&mut w);
        encode(CodeWidth::U32, 36, &w.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{CodeWidth, decode};
    use bytes::BytesMut;

    fn roundtrip(frame: Bytes) -> FromServer {
        let mut buf = BytesMut::from(&frame[..]);
        let f = decode(&mut buf, CodeWidth::U32, 1 << 20).unwrap().unwrap();
        FromServer::decode(f.code, f.body).unwrap()
    }

    /// The worked example in the protocol documentation, byte for byte.
    #[test]
    fn login_matches_the_documented_example() {
        let got = ToServer::Login {
            username: "username".into(),
            password: "password".into(),
        }
        .encode();
        let mut want = Vec::new();
        want.extend_from_slice(&72u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        for s in ["username", "password"] {
            want.extend_from_slice(&(s.len() as u32).to_le_bytes());
            want.extend_from_slice(s.as_bytes());
        }
        want.extend_from_slice(&MAJOR_VERSION.to_le_bytes());
        want.extend_from_slice(&32u32.to_le_bytes());
        want.extend_from_slice(b"d51c9a7e9353746a6020f9602d452929");
        want.extend_from_slice(&MINOR_VERSION.to_le_bytes());
        assert_eq!(&got[..], &want[..]);
    }

    #[test]
    fn decodes_login_rejection_with_detail() {
        let mut w = Writer::new();
        w.bool(false).str("INVALIDUSERNAME").str("Nick too long.");
        let frame = encode(CodeWidth::U32, 1, &w.finish());
        assert_eq!(
            roundtrip(frame),
            FromServer::LoginRejected {
                reason: "INVALIDUSERNAME".into(),
                detail: Some("Nick too long.".into())
            }
        );
    }

    #[test]
    fn decodes_mock_messages() {
        let ip: Ipv4Addr = "10.1.2.3".parse().unwrap();
        assert_eq!(
            roundtrip(mock::connect_to_peer("bob", ConnKind::File, ip, 2234, 99)),
            FromServer::ConnectToPeer {
                username: "bob".into(),
                kind: ConnKind::File,
                ip,
                port: 2234,
                token: 99,
                privileged: false
            }
        );
        assert_eq!(
            roundtrip(mock::peer_address("bob", ip, 2234)),
            FromServer::PeerAddress {
                username: "bob".into(),
                ip,
                port: 2234,
                obfuscated_port: None
            }
        );
    }

    #[test]
    fn decodes_a_private_room_join() {
        let mut w = Writer::new();
        w.str("den")
            .strings(&["a", "b"])
            .u32(2)
            .u32(2)
            .u32(1)
            .u32(2);
        for _ in 0..2 {
            w.u32(100).u32(1).u32(0).u32(10).u32(2);
        }
        w.u32(2)
            .u32(0)
            .u32(1)
            .strings(&["GB", ""])
            .str("a")
            .strings(&["b"]);
        match roundtrip(encode(CodeWidth::U32, 14, &w.finish())) {
            FromServer::JoinRoom {
                room,
                users,
                owner,
                operators,
            } => {
                assert_eq!(room, "den");
                assert_eq!(users.len(), 2);
                assert_eq!(users[0].country.as_deref(), Some("GB"));
                assert_eq!(users[1].country, None);
                assert!(users[1].slots_full);
                assert_eq!(owner.as_deref(), Some("a"));
                assert_eq!(operators, ["b"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_codes_do_not_fail() {
        assert!(matches!(
            FromServer::decode(9999, Bytes::from_static(b"x")).unwrap(),
            FromServer::Unknown { code: 9999, .. }
        ));
    }
}
