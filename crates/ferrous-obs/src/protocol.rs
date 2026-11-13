//! Messages between Ferrous and `ferrous-obs-host`.

use serde::{Deserialize, Serialize};

/// Ferrous -> host (one JSON object per line on the host's stdin).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command_ {
    List,
    /// Creates the source this host runs (once).
    Create { id: String, settings: serde_json::Value },
    Properties,
    Update { settings: serde_json::Value },
    Click { property: String },
    Quit,
}

/// Host -> Ferrous (one JSON object per line on the host's stdout).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ready { version: String },
    Types { inputs: Vec<TypeInfo>, filters: Vec<TypeInfo> },
    Created { width: u32, height: u32 },
    Properties { properties: Vec<Property>, settings: serde_json::Value },
    Ok,
    Error { message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TypeInfo {
    pub id: String,
    pub name: String,
    /// Has video / audio.
    pub video: bool,
    pub audio: bool,
}

/// One entry of a plugin's settings form.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Property {
    pub name: String,
    pub description: String,
    pub visible: bool,
    pub enabled: bool,
    #[serde(flatten)]
    pub kind: PropertyKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PropertyKind {
    Bool,
    Int { min: i64, max: i64, step: i64, suffix: String },
    Float { min: f64, max: f64, step: f64, suffix: String },
    Text { multiline: bool, password: bool, info: bool },
    Path { filter: String, directory: bool, save: bool },
    List { items: Vec<(String, serde_json::Value)> },
    Color { alpha: bool },
    Button,
    Font,
    Group { properties: Vec<Property> },
    Other { obs_type: i32 },
}

/// Media stream framing: `kind:u8, width:u32, height:u32, timestamp:u64, len:u32`, then `len` bytes.
pub const MEDIA_HEADER: usize = 21;
pub const KIND_VIDEO: u8 = 1;
pub const KIND_AUDIO: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MediaHeader {
    pub kind: u8,
    pub width: u32,
    pub height: u32,
    pub timestamp: u64,
    pub len: u32,
}

impl MediaHeader {
    pub fn to_bytes(&self) -> [u8; MEDIA_HEADER] {
        let mut b = [0u8; MEDIA_HEADER];
        b[0] = self.kind;
        b[1..5].copy_from_slice(&self.width.to_le_bytes());
        b[5..9].copy_from_slice(&self.height.to_le_bytes());
        b[9..17].copy_from_slice(&self.timestamp.to_le_bytes());
        b[17..21].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    pub fn parse(b: &[u8; MEDIA_HEADER]) -> Option<Self> {
        let h = MediaHeader {
            kind: b[0],
            width: u32::from_le_bytes(b[1..5].try_into().ok()?),
            height: u32::from_le_bytes(b[5..9].try_into().ok()?),
            timestamp: u64::from_le_bytes(b[9..17].try_into().ok()?),
            len: u32::from_le_bytes(b[17..21].try_into().ok()?),
        };
        // Sanity limit: 8K BGRA.
        (h.len <= 7680 * 4320 * 4).then_some(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shapes() {
        assert_eq!(serde_json::to_string(&Command_::List).unwrap(), r#"{"cmd":"list"}"#);
        let r: Reply = serde_json::from_str(r#"{"reply":"created","width":10,"height":20}"#).unwrap();
        assert_eq!(r, Reply::Created { width: 10, height: 20 });
        let p = Property { name: "w".into(), description: "Width".into(), visible: true, enabled: true, kind: PropertyKind::Int { min: 0, max: 10, step: 1, suffix: String::new() } };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""type":"int""#), "{s}");
        assert_eq!(serde_json::from_str::<Property>(&s).unwrap(), p);
    }

    #[test]
    fn media_header_round_trip() {
        let h = MediaHeader { kind: KIND_VIDEO, width: 1920, height: 1080, timestamp: 42, len: 1920 * 1080 * 4 };
        assert_eq!(MediaHeader::parse(&h.to_bytes()), Some(h));
    }
}
