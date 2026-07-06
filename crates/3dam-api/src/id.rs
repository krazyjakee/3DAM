//! Stable identifiers and the content hash.
//!
//! Ids are UUIDv7 (time-ordered → good B-tree locality at 1M+ rows, tech-spec 02 §2.1).
//! On the wire they render as their canonical hyphenated string. The content hash is a
//! raw BLAKE3 digest (tech-spec 02 §2.2); it serialises as lowercase hex.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Allocate a fresh time-ordered id (no DB round-trip).
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
            /// The 16 raw bytes, as stored in a `BLOB(16)` column.
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
            pub fn from_bytes(b: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(b))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

uuid_id!(
    /// Local id for a catalogued asset (also present for federated refs).
    AssetId
);
uuid_id!(
    /// A configured source (local FS / SFTP / SMB / federated peer).
    SourceId
);
uuid_id!(
    /// A background job (scan / analyse / convert / export).
    JobId
);
uuid_id!(
    /// A manual collection or smart folder.
    CollectionId
);
uuid_id!(
    /// A tag.
    TagId
);

/// Raw BLAKE3 digest over a file source's bytes. `None`-equivalent for federated assets
/// (they have no local bytes — tech-spec 02 §2.2). Serialises as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Self(out))
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}
impl std::fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ContentHash({})", self.to_hex())
    }
}
impl Serialize for ContentHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}
impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ContentHash::from_hex(&s).ok_or_else(|| serde::de::Error::custom("invalid content hash hex"))
    }
}
