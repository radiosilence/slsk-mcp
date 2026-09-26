//! The Soulseek wire protocol, as the Nicotine+ project documents it:
//! <https://nicotine-plus.org/doc/SLSKPROTOCOL.html>.
//!
//! Pure encoding and decoding over `bytes` — no sockets, no runtime — so the
//! engine can decode on whichever task owns the connection and forward frames
//! without copying. Obfuscated connections are not implemented, for the same
//! reason Nicotine+ gives: one unobfuscated port is all the network needs.

pub mod frame;
pub mod peer;
pub mod server;
pub mod wire;

pub use frame::{CodeWidth, Frame};
pub use wire::{DecodeError, RawStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnKind {
    Peer,
    File,
    Distributed,
}

impl ConnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Peer => "P",
            Self::File => "F",
            Self::Distributed => "D",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "P" => Some(Self::Peer),
            "F" => Some(Self::File),
            "D" => Some(Self::Distributed),
            _ => None,
        }
    }
}

/// Transfer rejection reasons other clients recognise, verbatim.
pub mod reason {
    pub const BANNED: &str = "Banned";
    pub const CANCELLED: &str = "Cancelled";
    pub const COMPLETE: &str = "Complete";
    pub const NOT_SHARED: &str = "File not shared.";
    pub const READ_ERROR: &str = "File read error.";
    pub const SHUTDOWN: &str = "Pending shutdown.";
    pub const QUEUED: &str = "Queued";
    pub const TOO_MANY_FILES: &str = "Too many files";
    pub const TOO_MANY_MEGABYTES: &str = "Too many megabytes";
}
